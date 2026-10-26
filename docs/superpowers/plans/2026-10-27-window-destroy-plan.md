# One authoritative destruction plan for window teardown

Date: 2026-10-27
Base: `fix/297-destroy-root` (master `04783d72` + uncommitted root guard and
`destroy_window_on_root_is_a_no_op`). Issue #297.

## Problem

#297 is a server panic in the KMS backend (`reparent_subwindow`, "subwindow …
not found"), preceded by "dst unresolvable" copy drops. `DestroyWindow(root)`
reproduces those symptoms: the backend loses every window while the core keeps
them. Whether the reporter's client actually sent it is not established; their
log will tell. The handler guard on this branch fixes that input.

The structural cause is that two layers decide independently what a destroy
destroys:

- `collect_destroy_order` (`core_loop/process_request/windows.rs:320`) walks the
  tree and builds `order`. **Every** side effect is driven by `order`: XI1 and
  core focus revert, UnmapNotify/DestroyNotify capture and fanout, Picture
  frees, Present purge, redirect release, XFIXES selection drops, subscription
  drops, attribute-pixmap release, backend `destroy_subwindow`.
- `ResourceTable::destroy_window_inner` (`resources/window.rs:298`) makes its
  own decision (it holds the X11 "root: no effect" guard) and returns the set
  it actually removed. Every caller discards that set
  (`let _ = state.resources.destroy_window(root)`).

For root the two disagreed: the resource layer removed nothing, everything else
tore down the whole tree. The handler guard fixes this one input; it does not
stop the next disagreement. The same sequence is also written out more than
once, and the copies have drifted (see "Current divergences").

## Goal

A destroy is described **once**, by the resource layer, as a plan. Every side
effect consumes that plan; the resource mutation executes it and decides
nothing. A window the resource layer won't destroy cannot have any side effect
applied to it, because it is not in any plan.

Non-goal: the backend's parallel `windows` store (`WindowGeometry`). That is
the step-3 projection work from
`docs/superpowers/findings/2026-06-18-pointer-stacking-dual-authority-diagnosis.md`.
This plan makes the core agree with itself; step 3 makes the backend unable to
disagree.

## Inventory: every window-destroy path

| Path | Entry | Builds order with | Resource mutation |
|---|---|---|---|
| A. explicit | `destroy_window_subtree` (`windows.rs:490`) — DestroyWindow, DestroySubwindows (`:2293`), screensaver window (`saver_dpms.rs:338`) | `windows.rs::collect_destroy_order` | `destroy_window(root)` |
| B. disconnect | `process_disconnect_reporting` (`process_disconnect.rs:~270-335`) | copy of `collect_destroy_order` in `process_disconnect.rs:32` | `destroy_window(root)` per owned root |
| C. zombie | `destroy_zombie_resources_reporting` (`process_disconnect.rs:~830-900`) | same copy | same |
| D. COW | `teardown_overlay` (`composite_overlay.rs:~123-210`), from ReleaseOverlayWindow (`composite_damage.rs:436`) and `release_client_overlay_claims` on disconnect (`composite_overlay.rs:259`) | none — the COW only | `destroy_cow_resource()` |

`PendingDestroy` is defined twice (A and B/C).

## Current divergences between the paths (from source)

Each is a static fact read from the code on `04783d72`; whether it is reachable
or matters at runtime is not established. Each needs an Xorg answer before it is
unified (phase 0).

1. **Unmap of the top window.** A sends UnmapNotify *before* the teardown,
   computes clip lists, and sends Expose for what the window covered
   (`dix/window.c:2856-2866`). B/C call `unmap_window` + `windows_restructured`
   but send **no exposures**, and send UnmapNotify *after* the resource is gone,
   from `fanout_destroy_sequence`.
2. **Focus.** A reverts XI1 focus and core focus. B/C revert XI1 focus only;
   `revert_core_focus_from` is not called in `process_disconnect.rs`.
3. **Grabs on a destroyed window.** A calls `release_core_grabs_for_unviewable`
   (`grabs.rs:1412`), which deactivates *any* client's active grab whose grab
   or confine-to window is no longer viewable. B/C release the departing
   client's own grabs, a different case. A grab held by a surviving client on a
   window that dies with the departing client's subtree is not released by B/C
   in that file.
4. **XFIXES selections.** A drops selections owned by destroyed windows
   (`SelectionWindowDestroy`). B fires `SelectionClientClose` for the client and
   filters `selections` by `dead_windows`. Which notify a surviving client's
   selection on a window under the departing client's tree should get is open.
5. **Host event masks.** A clears the host pointer/subwindow masks before
   `destroy_subwindow`; B/C do not.
6. **Attribute pixmaps.** A frees them with its own orphan loop; B/C collect them
   and free through `free_orphaned_host_pixmaps` (the single gate, per #133),
   which also fills `HostPixmapFrees`.
7. **COW children.** D's `destroy_cow_resource` removes the COW record and its
   slot in root's children but does **not** destroy the COW's children: their
   records stay in the table with `parent == COW`. Xorg frees the overlay through
   `DeleteWindow`, which crushes the subtree. On disconnect the compositor's own
   windows are destroyed by B before `release_client_overlay_claims` runs, so
   reaching this needs a child owned by another client, or an explicit
   ReleaseOverlayWindow with the children still there. Unmeasured.
8. **COW destroyed by an ordinary request.** `DestroyWindow(COW)` and
   `DestroySubwindows(root)` reach the COW through A, not D. A removes the core
   record and calls `destroy_subwindow(0x103)`, but nothing clears the backend's
   `cow_id`, the direct-scanout teardown is skipped, and `cow_claims` stays
   non-empty. Because GetOverlayWindow materializes only when `cow_claims` is
   empty (`composite_damage.rs:371`), the next claimant gets back `0x103` with
   no core record behind it. In Xorg, `compDestroyWindow`
   (`composite/compwindow.c:624-626`) clears `pOverlayWin` and leaves the claim
   list (`pOverlayClients`) alone, and `ProcCompositeGetOverlayWindow`
   (`compext.c:303`) creates a new overlay whenever `pOverlayWin` is NULL.

## Design

### The plan type (resource layer)

```rust
/// What one destroy will remove, decided by the resource table alone.
/// `windows` is Xorg `CrushTree` order: inferiors first, topmost first,
/// the subtree's top window last.
pub struct DestroyPlan {
    top: ResourceId,
    windows: Vec<ResourceId>,
}

impl ResourceTable {
    /// `None` if `id` is not a window or may not be destroyed (root: Xorg
    /// `ProcDestroyWindow` frees only a window with a parent).
    pub fn plan_destroy(&self, id: ResourceId) -> Option<DestroyPlan>;

    /// Remove exactly `plan.windows`. Decides nothing.
    pub fn apply_destroy(&mut self, plan: &DestroyPlan);
}
```

- `plan_destroy` is the only place the root rule lives. The guard moves out of
  `destroy_window_inner` and out of `handle_destroy_window`.
- `DestroyPlan` has no public constructor: a plan can only come from the table.
  The COW is planned like any window: `plan_destroy(COMPOSITE_OVERLAY_WINDOW)`
  yields the COW and its subtree.
- **Stale plans are an internal error.** A plan is valid only for the state it
  was built from. The executor builds and applies it in one call, and the steps
  in between (event capture and fanout, unmap, exposure, Picture/Present/
  redirect/selection release) never create, destroy or reparent a window, so a
  stale plan is impossible by construction. `apply_destroy` checks that every
  planned window exists and is still linked as planned, and that the top's parent
  still lists it. A mismatch is a bug: it panics with the plan and the
  actual state, as other cross-layer invariants do today. There is no
  partial-execution mode. Routing such invariant failures through an orderly
  shutdown instead of a panic is the audit's separate item
  (`docs/code-quality-audit-2026-07-26.md`), not this plan.
- `destroy_window` and `destroy_cow_resource` are removed (phase 4).

### One executor (core loop)

```rust
/// What one executed plan destroyed, for the caller's cleanup that is not
/// window-scoped (disconnect: damage, shape, Present subscriptions, …).
pub(crate) struct DestroyOutcome {
    /// Exactly `plan.windows`, CrushTree order.
    pub windows: Vec<ResourceId>,
    /// Background/border host pixmaps the destroyed windows held; candidates
    /// for the orphan gate, not yet freed.
    pub attr_pixmaps: Vec<u32>,
}

pub(crate) fn execute_destroy(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    prepared: PreparedDestroy,
) -> DestroyOutcome;
```

`PreparedDestroy` wraps the plan and is only made by `DestroyPlan::prepare`
(see "The COW"); the executor never sees a bare plan.

It is today's `destroy_window_subtree` body, with every `order` replaced by
`plan.windows` and `destroy_window(root)` replaced by `apply_destroy(&plan)`.
The order of steps is kept (old tree readable for focus revert, event capture,
clip lists and Picture frees; subscriptions dropped after apply; backend
teardown and DestroyNotify last).

**Attribute pixmaps leave the executor.** It returns them instead of freeing
them, and every caller frees through `free_orphaned_host_pixmaps`:

- A calls it right after the executor, with no report.
- B/C extend their candidate list (with the client's own freed pixmaps and last
  names, after `remove_non_window_resources_owned_by`) and keep passing
  `host_pixmap_frees`, so `HostPixmapFrees` reporting is unchanged.

That settles divergence 6 and leaves one orphan gate, per #133.

**Disconnect keeps the exact destroyed set.** B/C collect `outcome.windows`
from each owned root into `all_destroyed` / `dead_windows`, which the rest of
`process_disconnect` already uses (damage, shape, XFIXES, Present, redirect,
selection cleanup at `process_disconnect.rs:359-648`). Those sets include
windows of *surviving* clients under the departing client's windows; they come
from the plan, not from an ownership filter.

### Callers

- DestroyWindow: `if let Some(plan) = plan_destroy(w)`, then
  `plan.prepare(backend)?` (BadAlloc on `Err`), then `execute_destroy`.
  Root yields `None`: a successful no-op with no special case.
- DestroySubwindows: one plan per child; all are prepared before the unmap
  pass, then executed (see "The COW").
- Screensaver: plan, prepare, execute. It cannot contain the COW, so `prepare`
  does nothing and cannot fail.
- Disconnect/zombie: one plan per owned root, prepared and executed in turn.
  These plans cannot contain the COW either (it is server-owned, parented to
  root), so `prepare` cannot fail. Xorg
  `FreeClientResources` deletes each window resource through `DeleteWindow`, so
  each root's events go out before the next root's; phase 0 confirms the order.
  The client-level work (resources, SYNC, GLX, …) stays in `process_disconnect`.
- COW: below.

The duplicate `collect_destroy_order` and `PendingDestroy` are deleted.

### The COW

The COW cannot take the executor's ordinary backend step
(`unregister_host_window` + `destroy_subwindow`). The backend's
`release_overlay_window` does two things:

- **Fallible preparation.** On KMS, while the M2 direct scanout is active,
  `defer_cow_release` (`kms/platform_hooks.rs:314`) must allocate the shadow
  (`materialize_direct_shadow_for_unflip`) before a scanned-out buffer can go.
  On `Err` the overlay stays up and core keeps the caller's claim.
- **Deferred retirement.** With direct scanout active it requests an unflip and
  sets `deferred_cow_release`. The backend `windows` entry and storage stay
  until the scanout retires (`finish_deferred_cow_release`, `kms/scanout.rs:1455`).
  A GetOverlayWindow before then reuses that identity
  (`portable/overlay.rs:63-82`).

Split the trait method in two, at the boundary that already exists inside it:

```rust
/// Everything that can fail. Leaves core and backend state unchanged on Err.
/// Ok(false): this backend has no COW (v1, ynest, the trait default).
fn prepare_overlay_release(&mut self, origin: Option<OriginContext>) -> io::Result<bool>;

/// Cannot fail. Retires the COW now, or defers it behind direct scanout
/// exactly as `release_overlay_window` does today.
fn retire_overlay_window(&mut self, origin: Option<OriginContext>);
```

**Every plan that contains the COW takes the overlay path, whoever asked for
it.** Three requests can destroy the COW: ReleaseOverlayWindow on the last
claim, `DestroyWindow(COW)`, and `DestroySubwindows(root)`. A disconnect cannot,
because the COW is owned by the server and its parent is root. The overlay
handling therefore lives in the plan and the executor, not in
`teardown_overlay`:

- `plan_destroy` records whether the plan contains the COW.
- The executor never takes a bare `DestroyPlan`. A plan becomes executable
  through
  `DestroyPlan::prepare(self, backend) -> io::Result<PreparedDestroy>`, which
  calls `prepare_overlay_release()` when the plan contains the COW and does
  nothing otherwise. `execute_destroy` takes a `PreparedDestroy`, so no caller
  can skip preparation.
- For the COW's own backend step, the executor calls `retire_overlay_window()`
  instead of `unregister_host_window` + `destroy_subwindow`. The COW's
  `unregister_host_window` stays after the unmap crossings, as today. The
  children get the ordinary backend teardown, before the COW's, while the COW
  still exists in the backend (CrushTree order puts them first).
- After `apply_destroy`, the executor calls `destroy_cow_input_shape` when the
  plan contained the COW.

**Existence and claims are separate, as in Xorg.** The COW exists when its core
record exists; `cow_claims` only counts clients. This fixes divergence 8.

- **GetOverlayWindow** adds a claim, and materializes the COW when the core
  record is absent, whatever the claim count. This is Xorg `compext.c:303`. The
  backend's deferred-retirement reuse (`portable/overlay.rs:63-82`) covers the
  case where the old COW has not retired yet.
- **DestroyWindow(COW) / DestroySubwindows(root)** destroy the COW and its
  subtree and leave `cow_claims` untouched (Xorg `compDestroyWindow`). The
  claimants' next GetOverlayWindow creates a new overlay.
- **The last ReleaseOverlayWindow** builds `plan_destroy(COMPOSITE_OVERLAY_WINDOW)`
  if the record exists and runs it; if the COW is already gone it just drops the
  claim (Xorg `compDestroyOverlayWindow` → `FreeResource` of a freed id is a
  no-op).

**Failure handling.** Xorg cannot fail here; we can, in `prepare`. The X11
protocol allows Alloc from any request ("the explicit listing of Alloc in
requests only covers allocation errors at a very coarse level"), so:

| Entry | On `prepare` failure |
|---|---|
| ReleaseOverlayWindow (last claim) | BadAlloc, claim kept, nothing changed (today's behaviour) |
| Disconnect of the last claimant | claim dropped, `cow_teardown_failed` (today's behaviour) |
| `DestroyWindow(COW)` | BadAlloc, nothing changed: no event, COW and subtree intact |
| `DestroySubwindows(root)` | BadAlloc, nothing changed: no child unmapped or destroyed |

`DestroySubwindows` builds and prepares **all** child plans before its unmap
pass (`unmap_subwindows_with_delta`), then executes them. Xorg deletes the
children bottom-up, so the COW, as the topmost child, comes last, and failing
there would leave the request half-done.

`prepare` only allocates the shadow buffer; it is always followed by execution
in the same request with nothing fallible in between. `teardown_overlay` shrinks
to claim handling around `plan → prepare → execute`. Its hand-written COW
UnmapNotify/DestroyNotify, Picture and Present code goes away, because the
executor does all of that.

## Phases

### Phase 0 — establish the reference (no code)

For each divergence 1-8, cite the Xorg code path (`dix/window.c` `DeleteWindow`,
`CrushTree`, `dix/resource.c` `FreeClientResources`, `dix/events.c`
`DeleteWindowFromAnyEvents`, `xfixes/select.c`, `composite/compoverlay.c`) and
record the Xorg behaviour in this doc. Specifically:

- whether core focus is reverted for B/C anywhere outside
  `process_disconnect.rs`;
- a grab held by a surviving client on a window destroyed by another client's
  disconnect (`DeleteWindowFromAnyEvents` deactivates it);
- the per-root event order on disconnect;
- the selection notify for a surviving client's selection window that dies with
  another client's subtree.

Output: a table of "Xorg does X; A/B/C/D do Y; the executor does X".

### Phase 1 — plan type and executor, explicit path only

Add `DestroyPlan`, `plan_destroy`, `apply_destroy`, `PreparedDestroy`,
`DestroyOutcome`, `execute_destroy`. Until phase 3, `prepare` does nothing
for any plan, so `DestroyWindow(COW)` keeps today's behaviour (divergence 8)
until phase 3 fixes it. Unit tests in `resources/tests/window.rs`:

- the plan of root is `None`;
- the plan of an unknown id is `None`;
- the plan order is CrushTree order (expected order written from `dix/window.c`
  `CrushTree`, not from our output);
- `apply_destroy` removes exactly the planned set and unlinks the top window
  from its parent;
- `apply_destroy` panics on a stale plan (a planned window already removed);
- the plan of the COW contains its children.

Switch A to the executor; its attribute pixmaps go through
`free_orphaned_host_pixmaps`. Drop the handler guard; keep
`destroy_window_on_root_is_a_no_op`, which must stay green with the guard gone.

### Phase 2 — B and C on the executor

Route disconnect and zombie through `execute_destroy`, applying the phase-0
answers for divergences 1-5. Tests:

- A departing client's window with a **surviving client's child**: the child is
  destroyed; the survivor's subscriptions on it, its damage object, shape
  selection and Present subscription on it are cleaned up; the survivor's other
  windows and resources are untouched.
- The same with the survivor holding an active grab on the child (divergence 3,
  expected result from phase 0).
- `HostPixmapFrees` for a disconnect that frees a border-only tile reports the
  same `freed`/`deferred` sets as before the change.
- One test for each other divergence whose behaviour changes, with the expected
  events from the phase-0 Xorg reading.

Existing disconnect and zombie tests stay green or change only where phase 0
says they encoded a divergence.

### Phase 3 — COW on the executor

Split `release_overlay_window` into `prepare_overlay_release` /
`retire_overlay_window` (trait default, KMS, recording backend). Add
`PreparedDestroy`, route `teardown_overlay`, `DestroyWindow` and
`DestroySubwindows` through it, and key GetOverlayWindow materialization on
whether the COW record exists. Each test below runs with direct scanout active
unless it says otherwise:

- **Release failure:** `prepare_overlay_release` returns `Err`. The core COW
  record, its children, the claim and the backend COW are all unchanged, and no
  event went out.
- **DestroyWindow(COW):** the COW and its children are destroyed with their
  events, the backend retirement is deferred, and `cow_claims` is unchanged. The
  claimant's next GetOverlayWindow returns a COW that has a core record.
- **DestroyWindow(COW), prepare fails:** BadAlloc; nothing changed, no event.
- **DestroySubwindows(root):** every child is destroyed, including the COW and
  its subtree, through the overlay path. The claims are unchanged.
- **DestroySubwindows(root), prepare fails:** BadAlloc; no child unmapped or
  destroyed, no event.
- **GetOverlayWindow recreation fails:** with the COW destroyed and claims
  still held, a further GetOverlayWindow whose materialization fails returns
  BadAlloc, rolls back only its own new claim, keeps the existing claims, and
  leaves no core COW record (no partial window). Today's rollback in
  `composite_damage.rs:371-377` keys on the first claim; it must key on the
  record being absent.
- **Release after the COW was destroyed:** the last ReleaseOverlayWindow
  succeeds and drops the claim, and no teardown runs.
- **Deferred retirement:** release while direct scanout is active. The core
  resources are gone and the children's backend windows are destroyed, but the
  backend COW entry and storage stay until the scanout retires, then go.
- **Reacquire before retirement:** GetOverlayWindow while the release is
  deferred reuses the backend identity, rematerializes the core record, and does
  not bring back the old children.
- **Child under the COW:** release the last claim with a child under the COW.
  The child is destroyed, its DestroyNotify goes out, and no record is left
  with `parent == COW`.

The backend-side tests extend the existing
`release_overlay_window_final_release_tears_down_full_backend_state`
(`kms/render/backend/tests/paint_target.rs:2910`).

### Phase 4 — remove the leftovers

Delete `destroy_window`, `destroy_cow_resource`, `release_overlay_window`, the
duplicate `collect_destroy_order` and `PendingDestroy`. The one test caller of
`destroy_cow_resource` (`resources/tests/window.rs:1198`) moves to a plan. The
`trait_def.rs:~1811-1840` doc comment is rewritten for the split.

## Verification

- Per phase: `cargo +nightly fmt`, `cargo clippy` (workspace, all targets),
  `cargo test`.
- Lavapipe/vng suite (CI).
- HW smoke before merge:
  - a compositing desktop (Cinnamon or Plasma: the COW path);
  - compositor restart (`--replace`), including with a fullscreen client
    scanned out directly (the deferred COW retirement);
  - client kill (`xkill`);
  - a RetainPermanent client followed by `KillClient` (the zombie path).
- #297: ask the reporter to run the branch (after jos approves the message).

## Risks

- **Behaviour change on disconnect.** Divergences 1-3 may add events (Expose,
  FocusIn/Out, grab release) that disconnects never sent before. That is what
  Xorg does if phase 0 confirms it, but WMs see it at every client exit, so it
  needs the HW smoke above.
- **COW split.** The prepare/retire boundary must leave nothing irreversible in
  `prepare`. Today the shadow allocation is the only fallible step, and it
  already sits before any teardown inside `defer_cow_release`.
- **Size.** Phases 1 and 4 are mechanical; 2 and 3 carry the behaviour changes
  and should be reviewable on their own. One branch (one issue), one commit per
  phase, FF-merged.
