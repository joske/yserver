# Access control layer, PR 3a — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The resource and property hooks, the context's ownership resolver, and their check sites for every core
protocol request, with no change any client can observe while no policy is installed.

**Architecture:** `AccessPolicy` gains `resource` and `property`, both default allow. `AccessContext` gains the
facts the spec lists for these hooks: the owner of any checked object (its own resolver, separate from
`resource_owner`), whether a window is a root, and atom names. `ServerState` gains `check_resource` and
`check_property` on the existing first-denial loop. Each core request asks them at its operand validation, in the
order yserver validates today, and interprets a denial as the spec's denial table says.

**Tech Stack:** Rust, crate `yserver-core`; std only (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`. PR 3 is the third row of its series
table ("Resource and property checks, including creation and several-target requests"). PR 3 is split into
three plans that ship as one PR: **3a** (this plan: interface, resolver, core requests and window properties),
**3b** (RENDER, SHAPE, XFIXES, Composite, DAMAGE, and the window/cursor operands of XI, XKB and XTEST) and **3c**
(SYNC, Present, MIT-SHM, DRI3, GLX, RANDR including output properties, RECORD, XRes, Xinerama, MIT-SCREEN-SAVER).
PRs 1 (#328) and 2 (#334) provide `access.rs`, `first_denial`, `policies_installed` and the test policies in
`process_request/tests/extensions.rs`.

**Source inventory:** `~/Projects/claude-scratch/access-control/pr3/inventory.md` (codex, read-only, at
`8c99a1e3`). Line numbers below are from that commit; the implementer locates sites by function, not by line.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence. The existing
  suite passes unmodified. The one deliberate exception is Task 1, a bug fix with its own tests.
- Check helpers return `Allow` without a dynamic call, without building an `AccessContext` and without resolving an
  owner when the policy list is empty (`policies_installed()` first, as PR 2).
- No environment variable, no `cfg(test)` inside production function bodies. Test policies live in test modules.
- Reuse first: the new helpers use `first_denial`; existing convergence helpers (`validate_drawable_and_gc`,
  `validate_drawable_only`, `validate_gc_only`) gain the check instead of each drawing handler; owners come from the
  owner each store already keeps.
- `resource_owner` (`resources/cleanup.rs:219`) does not change: `KillClient`'s `BadValue` depends on its misses
  (spec, "Ownership").
- Operand validation order is observable: checks follow yserver's existing order and never consolidate or reorder
  it, even where XLibre's order differs (the inventory marks those rows "Order").
- Licensing: Xorg and XLibre are MIT and may be read; no copyleft source is consulted. No code is copied.
- Tests exercise production paths: every request goes through `process_request` (or `run_core` where a test needs
  the reader). Every new test names the mutation that makes it fail. Suites run under CPU load.

## Scope decisions (rulings for this plan)

These were decided with the user on 2026-11-05 or follow from the spec; each one is binding for 3b and 3c too.

- **The policy's error goes out verbatim.** A whole-request resource or property denial reports the policy's
  `DenyError` code and value, as the spec's denial table says. XLibre turns a `BadValue` from the hook into the
  type's not-found error (`dix/resource.c:1257`); yserver does not.
- **Every derived target gets its own check** (spec, "Where the checks go", as written). In 3a the derived targets
  are the children `DestroySubwindows`, `MapSubwindows` and `UnmapSubwindows` act on, and the child
  `CirculateWindow` chooses. Descendants that go with their ancestor (the subtree of a destroyed, mapped or
  unmapped window) are consequences, not targets, and are not checked, as in Xorg's `DestroyWindow`.
- **A derived-target denial never produces an error.**
  - `DestroySubwindows`: the spec's Xorg order (bulk unmap, then check and destroy each child; a denial stops the
    loop; `Success`).
  - `MapSubwindows` / `UnmapSubwindows`: a denied child is skipped and stays as it is, the loop continues, and
    the request succeeds. This is the per-window rule `MapWindow` has in XLibre (below), applied to each child.
  - `CirculateWindow`: a denied chosen child is not restacked, and the request succeeds.
- **`MapWindow` follows XLibre.** A denied `Show` maps nothing and the request returns `Success`
  (`dix/window.c:2641`). The same holds for the remap at the end of `ReparentWindow`: the reparent stays done and
  the window stays unmapped.
- **No `Post` property check.** XLibre's post-write content check (`dix/property.c:441`) inspects content; content
  rewriting is out of the series' scope (spec, Non-goals).
- **An operand yserver does not validate today** (inventory: "validation missing", e.g. CreateGC's drawable,
  CopyGC, the colormap of `AllocColor`) is checked only when its id resolves to an object of the expected type.
  An id that does not resolve keeps today's behaviour exactly; no new validity errors are added. The check sits
  before the handler's first effect (backend call, state mutation or reply).
- **Creation.** Every request that creates a resource asks the resource hook for the new id with
  `AccessMode::CREATE` (plus the extra mode in the table) and a `Creation` record: the parent window or drawable
  the request names, or none, and the requesting client as owner. It runs after every existing validation of the
  request and before the first allocation. A denial creates nothing and reports the policy's error.
- **Server work is not checked.** `check_resource` and `check_property` return `Allow` without asking when the
  client is `SERVER_OWNER` (`ClientId(0)`, `resources/mod.rs:29`). Real clients are numbered from 1
  (`poll_tokens.rs`, `ClientIdAllocator::new`). This is how the screensaver's calls to the core handlers
  (`saver_dpms.rs:304-317`) stay unchecked; hit testing, clipping, damage, destroy execution, disconnect and reset
  never call the helpers.
- **Request opcodes.** Every check made while running a request passes `Some(RequestOpcodes::for_request(header))`:
  for a core request (major < 128) the minor is 0, as Xorg's `client->minorOp` (`dix/dispatch.c:532`), because the
  data byte of a core request is a parameter; for an extension request it is the data byte, as PR 2's dispatch
  check. A core request's denial error keeps minor 0. (Corrected during Task 5; the first version said
  `header.data` for core requests too.)
- **Modes** are XLibre's at each site (`include/dixaccess.h`), taken from the inventory's section 2. `AccessMode`
  gains only the bits the 3a sites use; 3b and 3c add theirs.

## Interfaces

All in `crates/yserver-core/src/access.rs` unless stated.

- `AccessMode` keeps `READ` and `MANAGE` and their values, and gains: `WRITE`, `DESTROY`, `CREATE`, `GET_ATTR`,
  `SET_ATTR`, `LIST_PROP`, `GET_PROP`, `SET_PROP`, `LIST`, `ADD`, `REMOVE`, `HIDE`, `SHOW`, `BLEND`, `USE`,
  `RECEIVE`, `INSTALL`, `UNINSTALL`. Values are private bits; nothing outside the crate depends on them.
- `pub enum ResourceKind { Window, Pixmap, Gc, Font, Cursor, Colormap, Picture, GlyphSet, Region, PointerBarrier,
  SyncCounter, SyncAlarm, SyncFence, Damage, PresentEventSelection, ShmSegment, GlxContext, GlxDrawable,
  Dri3Syncobj, RecordContext, RandrOutput, RandrCrtc, RandrMode, RandrProvider }`. 3a defines all of them, so the
  resolver is complete once; 3b and 3c add only check sites.
- `pub struct Creation { pub parent: Option<ResourceId>, pub owner: ClientId }`.
- `pub struct ResourceTarget { pub id: ResourceId, pub kind: ResourceKind, pub creation: Option<Creation> }`.
- `AccessPolicy::resource(&self, ctx, client: ClientId, target: &ResourceTarget, mode: AccessMode) -> Decision`
  and `AccessPolicy::property(&self, ctx, client: ClientId, window: ResourceId, property: AtomId, mode:
  AccessMode) -> Decision`, both default `Allow`.
- `AccessContext::owner(&self, id: ResourceId, kind: ResourceKind) -> Option<ClientId>`: `None` when no object of
  that kind has that id. Server objects answer `SERVER_OWNER`: root and other server windows, default colormaps,
  the default font, SYNC system counters (recognized as `sync_await.rs:34` does, so their ids are not confused with
  the window ids `0x106`/`0x107`), and every RANDR object. Retained resources answer their original owner for as
  long as their store keeps them.
- `AccessContext::is_root(&self, window: ResourceId) -> bool` and `AccessContext::atom_name(&self, atom: AtomId) ->
  Option<&str>`.
- `ServerState::check_resource(&self, client, target: &ResourceTarget, mode, request: Option<RequestOpcodes>) ->
  Decision` and `ServerState::check_property(&self, client, window, property, mode, request) -> Decision`, in
  `server.rs` beside `check_server`, on `first_denial`, returning `Allow` early for `SERVER_OWNER`.

## Site table (core requests)

"After X" means immediately after the existing validation X succeeds. Where an operand has no validation today,
the ruling on missing validation applies. Denial is the policy's error unless the row says otherwise. Modes are
joined with `+`.

| Request | Checks, in yserver's order | Notes |
|---|---|---|
| 1 CreateWindow | parent `ADD` after parent lookup; background pixmap `READ`, colormap `USE`, cursor `USE`, border pixmap `READ`, each after its lookup, in yserver's order (bg → cmap → cursor → border); new window `CREATE+SET_ATTR`, `Creation { parent }` | |
| 2 ChangeWindowAttributes | window `RECEIVE` if the mask has `CWEventMask`, `+SET_ATTR` if it has any other bit; then the CW operands as CreateWindow | |
| 3 GetWindowAttributes | window `GET_ATTR` | |
| 4 DestroyWindow | window `DESTROY`; its parent `REMOVE`, if it has one | parent check is new; an id that resolves always has its parent |
| 5 DestroySubwindows | parent `REMOVE`; each child `DESTROY` in the destroy loop, immediately before its destroy | spec's Xorg order; derived-target ruling |
| 6 ChangeSaveSet | window `MANAGE` | before the mutable `ClientState` borrow (inventory §5) |
| 7 ReparentWindow | window `MANAGE`; new parent `ADD`; then, if the window is mapped, window `SHOW` for the remap | all three asked before any effect; see "ReparentWindow with a denied remap" |
| 8 MapWindow | window `SHOW` | denial: `Success`, nothing mapped |
| 9 MapSubwindows | parent `LIST`; each child `SHOW` in the map loop | derived-target ruling |
| 10 UnmapWindow | window `HIDE` | policy's error |
| 11 UnmapSubwindows | parent `LIST`; each child `HIDE` in the unmap loop | derived-target ruling |
| 12 ConfigureWindow | window `MANAGE+SET_ATTR`; sibling `GET_ATTR` | |
| 13 CirculateWindow | container `MANAGE`; chosen child `MANAGE+SET_ATTR` | derived-target ruling |
| 14 GetGeometry | drawable `GET_ATTR` on whichever target resolved, in the existing precedence: window, pixmap, then the GLX drawable record (`glx_pbuffer_geometry`, kind `GlxDrawable`) | the GLX fallback succeeds today for a pbuffer whose backing pixmap allocation failed (`glx.rs:628-672`); its zero-size guard (`247b7a42`) is kept |
| 15 QueryTree | window `LIST` | |
| 18 ChangeProperty | window `SET_PROP`; property hook: existing property `WRITE` (Replace) or `BLEND` (Append/Prepend), absent property `CREATE+WRITE` | property check after the type atom's validation |
| 19 DeleteProperty | window `SET_PROP`; existing property `DESTROY` | absent property: no property check, as XLibre |
| 20 GetProperty | window `GET_PROP` (`+SET_PROP` if delete); existing property `READ` (`+DESTROY` if delete) | |
| 21 ListProperties | window `LIST_PROP`; each property `GET_ATTR` | a denied property is left out and the reply succeeds; a missing window keeps today's empty reply |
| 22 SetSelectionOwner | owner window `SET_ATTR` (when not None) | the selection hook stays deferred (spec, Non-goals) |
| 24 ConvertSelection | requestor `SET_ATTR` | |
| 26 GrabPointer | confine `SET_ATTR`, grab window `SET_ATTR`, cursor `USE`, in yserver's order | |
| 28 GrabButton | grab window `SET_ATTR`, confine `SET_ATTR`, cursor `USE` | |
| 29 UngrabButton | window `READ` | |
| 30 ChangeActivePointerGrab | cursor `USE` | |
| 31 GrabKeyboard | window `SET_ATTR` | |
| 33 GrabKey | window `SET_ATTR` | |
| 34 UngrabKey | window `GET_ATTR` | |
| 38 QueryPointer | window `GET_ATTR` | |
| 39 GetMotionEvents | window `GET_ATTR` | |
| 40 TranslateCoordinates | src `GET_ATTR`, dst `GET_ATTR` | |
| 41 WarpPointer | dst `GET_ATTR`, then src `GET_ATTR` | |
| 42 SetInputFocus | focus window `SET_ATTR` | the device hook is PR 5 |
| 45 OpenFont | new font `CREATE`, `Creation { parent: None }` | |
| 46 CloseFont | font `DESTROY` | before `close_font` mutates (inventory §5) |
| 47 QueryFont, 48 QueryTextExtents | the fontable (font, or GC) `GET_ATTR` | |
| 53 CreatePixmap | drawable `GET_ATTR`; new pixmap `CREATE`, `Creation { parent: drawable }` | |
| 54 FreePixmap | pixmap `DESTROY` | before removal |
| 55 CreateGC | drawable `GET_ATTR`; tile `READ`, stipple `READ`, font `USE`, clip mask `READ`; new GC `CREATE+SET_ATTR`, `Creation { parent: drawable }` | |
| 56 ChangeGC | GC `SET_ATTR`; tile, stipple, font, clip mask as CreateGC | |
| 57 CopyGC | src `GET_ATTR`, dst `SET_ATTR` | before the helper copies |
| 58 SetDashes, 59 SetClipRectangles | GC `SET_ATTR` | |
| 60 FreeGC | GC `DESTROY` | |
| 61 ClearArea | window `WRITE` | |
| 62 CopyArea, 63 CopyPlane | src `READ`; dst `WRITE`; GC `USE`, in yserver's order (src, then dst and GC through the helper) | |
| 64–72 PolyPoint … PutImage, 76 ImageText8, 77 ImageText16 | drawable `WRITE`, GC `USE`, through `validate_drawable_and_gc` | |
| 73 GetImage | drawable `READ` | |
| 74 PolyText8, 75 PolyText16 | drawable `WRITE`, GC `USE`; each inline font `USE` in the existing font pass | all before any draw, as today |
| 78 CreateColormap | window `GET_ATTR`; new colormap `CREATE`, `Creation { parent: window }` | |
| 79 FreeColormap | colormap `DESTROY` | |
| 80 CopyColormapAndFree | src colormap `READ+REMOVE`; new colormap `CREATE`, `Creation { parent: None }` | |
| 81 InstallColormap | colormap `INSTALL` | the screen hook is not part of PR 3 (below) |
| 82 UninstallColormap | colormap `UNINSTALL` | |
| 83 ListInstalledColormaps | window `GET_ATTR` | |
| 84–87 AllocColor … AllocColorPlanes | colormap `ADD` | 86, 87 answer `BadAlloc` unconditionally today; the check runs first only when the colormap resolves |
| 88 FreeColors | colormap `REMOVE` | |
| 89 StoreColors, 90 StoreNamedColor | colormap `WRITE` | today `BadAccess` unconditionally; same rule as 86 |
| 91 QueryColors, 92 LookupColor | colormap `READ` | |
| 93 CreateCursor | source pixmap `READ`, mask pixmap `READ`; new cursor `CREATE`, `Creation { parent: None }` | |
| 94 CreateGlyphCursor | source font `USE`, mask font `USE`; new cursor `CREATE`, `Creation { parent: None }` | |
| 95 FreeCursor | cursor `DESTROY` | |
| 96 RecolorCursor | cursor `WRITE` | |
| 97 QueryBestSize | drawable `GET_ATTR` | |
| 113 KillClient | the named resource `GET_ATTR`, when `resource_owner` resolves it | kind = the store `resource_owner` found it in; the client check is PR 5 |
| 114 RotateProperties | window `SET_PROP`; each property `READ+WRITE`, in the existing per-item pass | all before rotation, as today |

Not in 3a: `GetAtomName` (atom only), `GetSelectionOwner` (selection hook, deferred), `SendEvent` (PR 4), the
screen hooks of Install/Uninstall/ListInstalledColormaps (the spec lists no screen hook; noted for PR 5's
"remaining server sites").

## Shared unmapping: checked requests versus consequences

`DestroySubwindows` and `UnmapSubwindows` share the child loop in `unmap_subwindows_with_delta`
(`windows.rs:2127`, `:2667`). The per-child checks belong to the requests, never to that helper:
`UnmapSubwindows` and `MapSubwindows` decide which children are allowed (asking `HIDE` / `SHOW` per child, in the
loop's order) and the helper acts on that set; `DestroySubwindows` passes every child, unchecked, because the spec
makes its bulk unmap unconditional and checks only `DESTROY`. The helper itself never calls a check.

## ReparentWindow with a denied remap

XLibre asks `SHOW` when it remaps, after the reparent (`dix/window.c:2565`, `:2641`); the policy sees the same
three questions in the same order when yserver asks them all before the first effect, which lets the handler pick
its path once:
- `SHOW` allowed (and with no policy): today's path unchanged, including its simulated unmap/remap
  (`windows.rs:469-526`) and its event order.
- `SHOW` denied: the window is unmapped first through the same transition a client `UnmapWindow` runs (subtree
  demotion, storage and redirect lifecycle, focus and grab handling, UnmapNotify; `windows.rs:2569-2637`), exactly
  once, then reparented with no remap. The client sees UnmapNotify, then ReparentNotify, no MapNotify, and no
  error, as Xorg's `ReparentWindow` followed by a denied `MapWindow`.

## Denial test contract

Every "deny" test row, in every task, asserts all of:
- the client reads exactly the site's denial result (the policy's error with the request's major and minor, or
  the row's success outcome);
- the `RecordingBackend` call log is unchanged by the request (no draw, configure, map, property, cursor or
  colormap call), except calls the row's permitted partial outcome names;
- a snapshot of the state the request would change is unchanged: for windows their attributes, geometry,
  stacking, mapping and properties; for GCs their values; for colormaps their entries and install state; for
  properties their type, format and data; the existence of an object a creation or destruction would add or
  remove.

The derived-target rows (DestroySubwindows, Map/UnmapSubwindows, CirculateWindow) and the ReparentWindow remap
assert their permitted partial outcomes exactly instead. A denied remap leaves the window in the state a client
`UnmapWindow` would leave it in.

## Review Focus

1. A check placed after an effect: e.g. CloseFont / FreePixmap, whose existence check is the removal itself, or
   ChangeSaveSet, which borrows `ClientState` first. A denial must leave the object and the state unchanged.
2. The screensaver path (`saver_dpms.rs:304-317`) with a policy that denies everything: the saver window is still
   created and mapped, because it runs as `SERVER_OWNER`.
3. `KillClient` on a colormap still answers `BadValue` with a policy installed (resolver separate from
   `resource_owner`).
4. ListProperties / QueryTree style replies with a denied entry are correctly framed (lengths and counts match the
   entries kept).
5. Region owners after another client's XFIXES write (Task 1): the region survives that client's disconnect.

---

### Task 1: Writes into an existing resource keep its owner

A bug fix, separate from the access layer, done first because the resolver reads these owners. Today a write into
an existing XFIXES region replaces its `owner` with the writer, and disconnect cleanup removes regions by owner
(`process_disconnect.rs:273`), so client A's region disappears when client B, who wrote into it, disconnects. The
same happens to a DAMAGE `Subtract` parts region and to a Present event selection.

**Files:** `core_loop/process_request/shape_xfixes.rs` (SetRegion, CopyRegion, Union/Intersect/SubtractRegion,
InvertRegion, TranslateRegion, RegionExtents, ExpandRegion), `core_loop/process_request/composite_damage.rs`
(DAMAGE Subtract parts), `core_loop/process_request/present_ext.rs` (SelectInput); tests in the existing
`process_request/tests` files for those extensions.

**Behaviour:**
- An XFIXES request that writes into an id that already holds a region keeps that region's owner. An id with no
  region keeps today's behaviour (the requester becomes owner): validity errors for missing destinations are not
  part of this task.
- DAMAGE `Subtract` writing into an existing parts region keeps its owner, the same way.
- Present `SelectInput` with an event id that already exists: if it belongs to another client or to another window,
  `BadMatch`, as Xorg (`present/present_event.c`, `present_select_input`); otherwise the mask is updated and the
  owner kept. Mask-0 removal is not part of this task (today a mask-0 selection is stored; recorded as a known
  difference in the PR body).

- [ ] **Step 1: Failing tests** (two clients through `process_request`; then the second client's disconnect through
  the production disconnect path):
  - `xfixes_write_into_other_clients_region_keeps_owner`: A creates region R; B runs CopyRegion, UnionRegion and
    TranslateRegion into R (one case each); B disconnects; `FetchRegion` by A still answers R's rectangles.
    Mutation: owner = writer.
  - `damage_subtract_parts_keeps_existing_region_owner`: same shape with A's parts region and B's Subtract.
  - `present_select_input_on_other_clients_event_id_is_bad_match`: B's SelectInput with A's eid answers `BadMatch`
    (error fields: code, value = eid as Xorg sends it, major, minor) and A's selection is unchanged. Mutation:
    overwrite.
- [ ] **Step 2:** run them, all fail.
- [ ] **Step 3:** implement. One helper per store if the same "keep the owner of an existing entry" logic repeats
  more than twice.
- [ ] **Step 4:** run them, all pass; the extension test files pass unmodified.
- [ ] **Step 5: Commit** `fix(core): writes into an existing XFIXES region, DAMAGE parts or Present selection keep its owner`.

### Task 2: The resource and property hooks, the resolver and the check helpers

**Files:** `access.rs`, `server.rs`; `resources/` for read-only lookups the resolver needs; new test file
`process_request/tests/access_resources.rs` registered in `tests/mod.rs`.

**Interfaces:** Produces everything in the Interfaces section. Consumes `first_denial`, `policies_installed`.

- [ ] **Step 1: Failing tests:**
  - `owner_resolver_answers_every_kind`: one row per `ResourceKind`; creates an object of that kind owned by client
    1 through `process_request` (or, for server objects, uses the existing one) and asserts `ctx.owner(id, kind)`
    from inside a recording policy's hook equals the expected owner; a second row per kind asserts `None` for an
    unused id. Server rows: root window, default colormap, default font, a SYNC system counter, a RANDR output,
    CRTC, mode and provider all answer `SERVER_OWNER`. Mutation: drop any one store from the resolver.
  - `owner_resolver_keeps_retained_owner`: client 1 sets `RetainPermanent`, creates a pixmap and a colormap,
    disconnects; the resolver still answers client 1 for both.
  - `kill_client_on_a_colormap_keeps_bad_value_with_a_policy`: allow-all policy installed; `KillClient` on a
    colormap answers `BadValue`, byte-identical to no policy. Mutation: resolve KillClient through the new resolver.
  - `server_owner_is_never_checked`: a deny-all policy; `check_resource` and `check_property` for `SERVER_OWNER`
    answer `Allow` and the recording shows no call.
  - `empty_policy_list_builds_no_context`: as PR 2's equivalent, for both helpers.

  The test policies reuse the shared recording and decision path PR 2 introduced (`ExtensionHook`/`check` in
  `tests/extensions.rs`): extend or move it so the resource and property hooks record through the same path,
  rather than adding a second copy.
- [ ] **Step 2:** run, fail. **Step 3:** implement. **Step 4:** run, pass; whole suite unmodified.
- [ ] **Step 5: Commit** `feat(core): access control PR 3a, Task 2: resource and property hooks, owner resolver`.

### Task 3: Drawing requests

**Files:** `core_loop/process_request.rs` (the three helpers), `drawing.rs`, `gc_pixmap_cursor.rs` (SetDashes,
SetClipRectangles, QueryBestSize), tests in `access_resources.rs`.

**Sites:** rows 61–77 and 97 of the site table. The three helpers take the client, the request opcodes and the two
modes; every caller passes them. CopyArea/CopyPlane keep their src-first order.

- [ ] **Step 1: Failing tests:**
  - `every_drawing_request_denial_reports_policy_error` (table-driven, one row per request 61–77): a policy denying
    the drawable (then, second pass, the GC) with `DenyError { code: BadAccess, value: 0x55 }`; the client reads
    exactly that error with the request's major and minor 0; the `RecordingBackend` recorded no draw call.
  - `every_drawing_request_asks_with_expected_target` (recording policy): client, id, kind, mode and
    `ctx.request()` per row, in yserver's order (CopyArea: src READ, dst WRITE, GC USE).
  - `poly_text_denied_inline_font_draws_nothing`: PolyText8 with two inline fonts, the second denied; no draw, the
    GC's font unchanged. Mutation: check fonts while drawing.
  - `several_policies_resource_order` (once, on PolyPoint): allow then deny and deny then allow are both denied, the
    first denier's error is reported, and a policy after a denier is not asked.
  - `revoked_drawable_denies_the_next_request`: a recording policy allows, then denies, the same drawable; the
    second PolyPoint is denied.
- [ ] **Step 2–4** as above. **Step 5: Commit** `… Task 3: drawing requests`.

### Task 4: Window creation, attributes and destruction

**Sites:** rows 1–6. DestroySubwindows: the check goes inside the existing destroy loop, immediately before each
child's destroy; a denial stops the loop, releases the remaining prepared destroys with no client-visible effect,
and the request returns no error.

- [ ] **Step 1: Failing tests:**
  - table-driven deny and ask tests for 1–6, as Task 3; CreateWindow asserts the `Creation { parent, owner }`
    record and that no window exists after a denial.
  - `destroy_subwindows_denied_child_stops_the_loop` (spec, Testing, "Several targets"): parent with children
    c1, c2, c3 (destroy order as yserver's loop); a policy denying c2's `DESTROY`. Every child is unmapped
    (UnmapNotify for each), c1 is destroyed (DestroyNotify), c2 and c3 still exist unmapped, and the request returns
    no error. With an empty list and with an allow-all policy, the bytes equal today's. Mutation: check before the
    bulk unmap; continue after a denial.
  - `destroy_subwindows_bulk_unmap_ignores_hide`: a policy denying every child's `HIDE` and c2's `DESTROY`: every
    child is still unmapped, c1 destroyed, c2 and c3 exist unmapped, no error. Mutation: put the `HIDE` check in
    the shared helper.
  - `destroy_window_checks_parent_remove`: denying the parent's `REMOVE` denies DestroyWindow with the policy's
    error and the window still exists.
- [ ] **Steps 2–5.** Commit `… Task 4: window creation, attributes and destruction`.

### Task 5: Window tree, mapping and pointer queries

**Sites:** rows 7–15 and 38–41.

- [ ] **Step 1: Failing tests:**
  - table-driven deny and ask tests for the whole-request rows.
  - `get_geometry_checks_glx_drawable_without_backing_pixmap`: a pbuffer whose backing allocation fails (the
    `RecordingBackend` refuses it), another client's GetGeometry on it with a policy denying `GlxDrawable`: the
    policy's error, no reply. Mutation: check only windows and pixmaps.
  - `map_window_denied_show_returns_success_and_maps_nothing`: no error, no MapNotify, the window stays unmapped.
    Mutation: report the policy's error.
  - `map_subwindows_skips_denied_child`: three unmapped children, the middle one denied; the other two get
    MapNotify, the middle one stays unmapped, no error. `unmap_subwindows_skips_denied_child`: the mirror image.
  - `reparent_denied_remap_leaves_window_reparented_and_unmapped`: a viewable window W with a viewable child C
    holding the input focus (and, second case, an active pointer grab on C), reparented between two viewable
    parents with W's remap `SHOW` denied. Asserts: UnmapNotify then ReparentNotify, no MapNotify, no error; W is a
    child of the new parent and unmapped; C is unviewable; focus and grab handled as after a client `UnmapWindow` of
    W; the `RecordingBackend` lifecycle calls equal those of `UnmapWindow` of W followed by the reparent, with no
    remap. Mutation: set only W's map state.
  - `reparent_with_allow_all_policy_keeps_todays_bytes`: the same setup with an allow-all policy and with none:
    events and backend calls equal today's.
  - `circulate_denied_child_is_not_restacked`: stacking order unchanged, no CirculateNotify, no error.
- [ ] **Steps 2–5.** Commit `… Task 5: window tree, mapping and pointer queries`.

### Task 6: GC, pixmap, cursor and font requests

**Sites:** rows 45–60 (except the drawing helpers already in Task 3), 93–96.

- [ ] **Step 1: Failing tests:**
  - table-driven deny and ask tests; creation rows assert the `Creation` record and that nothing was created.
  - `close_font_and_free_pixmap_denied_keep_the_object`: after a denied CloseFont / FreePixmap, the object is still
    usable by its owner (QueryFont answers; CopyArea from the pixmap succeeds). Mutation: check after removal.
  - `create_gc_unresolved_tile_keeps_todays_behaviour`: CreateGC naming a tile id that does not exist, with a
    deny-all policy that would deny any tile: the bytes equal today's (no new error). Mutation: check the id
    without resolving it.
- [ ] **Steps 2–5.** Commit `… Task 6: GC, pixmap, cursor and font requests`.

### Task 7: Colormaps

**Sites:** rows 78–92.

- [ ] **Step 1: Failing tests:** table-driven deny and ask tests for every row; 86, 87, 89, 90 assert that with an
  allow-all policy their unconditional error is byte-identical to today, and with a denying policy on a resolving
  colormap the policy's error comes first. `KillClient` colormap test lives in Task 2.
- [ ] **Steps 2–5.** Commit `… Task 7: colormaps`.

### Task 8: Grabs, focus, selections' windows, save set and KillClient's resource

**Sites:** rows 6 (save set, if not done in Task 4), 22, 24, 26–34, 42, 113.

- [ ] **Step 1: Failing tests:** table-driven deny and ask tests; `change_save_set_denied_leaves_save_set_unchanged`
  (the window is not reparented to root when the client later disconnects); `kill_client_resource_check_precedes_kill`
  (denied: the owner is still connected and the policy's error is reported).
- [ ] **Steps 2–5.** Commit `… Task 8: grabs, focus, selection windows and KillClient`.

### Task 9: Window properties

**Sites:** rows 18–21 and 114, with the property hook.

- [ ] **Step 1: Failing tests:**
  - table-driven deny and ask tests: window-level checks through `resource`, property-level through `property`,
    with the mode per row (Replace `WRITE`, Append `BLEND`, absent `CREATE+WRITE`; GetProperty delete adds
    `SET_PROP` / `DESTROY`).
  - `list_properties_omits_denied_property`: three properties, the middle one denied; the reply lists the other two
    and its length field matches. Mutation: deny the whole request.
  - `rotate_properties_denied_item_rotates_nothing`: all values unchanged, no PropertyNotify.
  - `server_property_writes_are_not_checked`: with a deny-all policy, `change_property_as_server` (rootless WM, the
    XKB rules property) still writes.
- [ ] **Steps 2–5.** Commit `… Task 9: window properties`.

### Task 10: Hot-path measurement, spec rows, gate

- [ ] **Step 1:** an `#[ignore]` test `bench_poly_point_with_and_without_policy` that runs 100 000 one-point
  PolyPoint requests through `process_request` with no policy and with one allow-all policy and prints ns per
  request for each. It asserts nothing; the numbers go to the PR body (spec, Testing, "Hot paths").
- [ ] **Step 2:** spec: in "A denial is interpreted by its site", add rows for `MapWindow` / `ReparentWindow` remap
  (`Success`, not mapped), `MapSubwindows` / `UnmapSubwindows` / `CirculateWindow` derived children (skipped,
  `Success`), and `ListProperties` (entry left out). In "Where the checks go", state that work run as
  `SERVER_OWNER` is server work, and that subtree descendants are consequences, not derived targets.
- [ ] **Step 3: Gate:** `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`, also with
  `--features tcp-transport` and `--features xdmcp`; FreeBSD clippy; `cargo check -p yserver-core --target
  x86_64-pc-windows-gnu`; `cargo test --workspace` and with `--features xdmcp`; each `crates/*/tests` file;
  `--features xdmcp -- --ignored` (lavapipe); `access_resources` 5 runs under `nproc` busy loops; `just man`.
- [ ] **Step 4: Commit** `docs(spec): access control PR 3a denial rows and server work`.
