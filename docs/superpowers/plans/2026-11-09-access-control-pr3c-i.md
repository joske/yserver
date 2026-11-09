# Access control layer, PR 3c-i — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resource checks at every request of SYNC, Present, DRI3, MIT-SHM and GLX that names a resource, with no
change any client can observe while no policy is installed.

**Architecture:** New check sites only. PRs 3a (#342) and 3b (#343) provide everything they call:
`ServerState::check_resource`, `AccessContext::owner` for every `ResourceKind` these extensions use (system SYNC
counters and RANDR CRTCs/providers answer `SERVER_OWNER`), `RequestOpcodes::for_request`, the
`validate_resource_access` / `validate_resource_if_present` / `validate_core_resource_if_present` helpers and
`return_on_access_denial!` in `core_loop/process_request.rs`, the drawable/GC convergence helpers, and every
`AccessMode` bit used below. Plan 3c-ii (RANDR, Xinerama, RECORD, ScreenSaver, XRes) follows this one.

**Tech Stack:** Rust, crate `yserver-core`; std only (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, series row 3. Plans 3a
(`2026-11-05-access-control-pr3a.md`), 3b (`2026-11-06-access-control-pr3b.md`) and the 3b review round
(`2026-11-07-access-control-pr3b-review1.md`) are the earlier parts of the same row.

**Source inventory:** `~/Projects/claude-scratch/access-control/pr3c/inventory.md` (taken at `f83615a4`), §1
(helpers, owners), §2 (SYNC, Present, DRI3, MIT-SHM, GLX and the additional GLX rows), §3, §5, §6. The implementer
locates sites by function name; line numbers drift.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence, replies,
  events, backend calls and damage. The existing suite passes unmodified.
- No new validity errors: an operand yserver does not validate today is checked only when its id resolves to an
  object of the expected kind (`validate_resource_if_present`), and an id that does not resolve keeps today's
  behaviour exactly. Missing new-ID validity (`BadIDChoice`), missing `GLXBadContext` / `GLXBadDrawable` errors and
  missing writable-segment checks stay missing; they are not added here.
- Each check runs immediately after its operand's own existing validation (lookup), before any later validation of
  other fields, as the 3b review round fixed (`2026-11-07-access-control-pr3b-review1.md`, Task 2). The order in
  which yserver validates operands is kept, even where XLibre's differs. An operand with no validation today is
  checked at the position XLibre looks it up relative to the request's other operands.
- Reuse first: every site calls the existing helpers; no second resolver, no new `AccessMode` bit, no new
  `ResourceKind`. Handler-local wrapper macros follow 3b's (`check_resource!`, `check_creation!` in `render.rs` /
  `shape_xfixes.rs`); a shared one is added only if two extension files would otherwise duplicate it.
- No environment variable, no `cfg(test)` inside production function bodies. Test policies live in test modules and
  reuse the shared recording and decision path (`process_request/tests/extensions.rs`, `ExtensionHook` / `check`,
  `RecordingExtensionPolicy`).
- Client checks (SYNC Set/GetPriority's client, XRes) and device checks are PR 5; send and receive (Present and SYNC
  event delivery) are PR 4.
- Licensing: Xorg and XLibre are MIT and may be read; no copyleft source is consulted. No code is copied.
- Tests exercise production paths through `process_request`. Every new test names the mutation that makes it fail.
  Suites run under CPU load. Test files stay under 6000 lines (`tools/check-layout.sh`).

## Scope decisions

Plan 3a's and 3b's scope decisions apply unchanged: the policy's error goes out verbatim; every derived target gets
its own check and a derived-target denial never produces an error; `SERVER_OWNER` work is not checked; modes are
XLibre's at each site; policies see an extension request's minor opcode from the data byte. The denial test contract
of plan 3a ("Denial test contract") and the row form of the 3b review round apply to every row here. Rulings new to
this plan:

- **Creations ask `CREATE` with a `Creation` record right before the first allocation or insertion** (the store
  insert, the backend import or allocation, the OS attach or mapping), after every existing validation that today
  runs before that point. Where yserver validates an operand only after its allocation (MIT-SHM `CreatePixmap`'s
  segment), the creation is asked before the allocation and the later operand keeps its check after its own
  validation; a denied creation allocates nothing. If that later check denies after the allocation succeeded, the
  handler frees the allocation (`backend.free_pixmap`) before answering the policy's error and registers nothing
  (review r1 M-1); the denial row for it expects exactly one allocation and its matching free in the backend log.
  The pre-existing leak on today's missing-segment `BadValue` path is unchanged here and reported as a finding.
- **A creation's parent** is the protocol-named drawable or window when it resolves, and `None` when it does not
  (yserver does not validate it today, so a non-resolving parent still creates). Objects with no protocol parent
  (SYNC counters and alarms, SHM segments, GLX contexts, GLX pbuffers) pass `None`.
- **Server objects are checked like any other target.** System SYNC counters and RANDR CRTCs and providers answer
  `SERVER_OWNER` through the existing resolver; a real client's request on one is still asked (only a request run
  as `SERVER_OWNER` skips the check).
- **A drawable operand that yserver does not validate** is checked as `Window` when it resolves as one, otherwise as
  `Pixmap` when it resolves as one, otherwise not at all (yserver's drawable resolution order). There is no
  `Drawable` kind.
- **GLX drawable operands with a core fallback** (`MakeCurrent` / `MakeContextCurrent` draw and read,
  `SwapBuffers`): a `GlxDrawable` that resolves is checked with XLibre's mode for it (`WRITE`); otherwise a core
  Window or Pixmap that resolves is checked with `GET_ATTR`, as XLibre's fallback lookup. yserver creates no implicit
  GLX drawable, so XLibre's implicit creation has no counterpart here.
- **GLX context tags, screens, visuals and FBConfigs are not resources** and are not checked.
- **Unsupported requests that touch no object are not checked:** GLX Render, RenderLarge, CopyContext, UseXFont, the
  unsupported vendor-private codes (QueryContextInfoEXT, CopySubBufferMESA, MakeCurrentReadSGI,
  CreateGLXPbufferSGIX, DestroyGLXPbufferSGIX, ChangeDrawableAttributesSGIX, GetDrawableAttributesSGIX) and WaitGL /
  WaitX keep today's replies and errors. They are listed so a reviewer can see they were considered.
- **GLX `CreatePixmap`'s post-publication `WRITE`** (XLibre's texture helper, whose denial leaves the resource and
  answers `Success`) has no counterpart: yserver has no such helper. Only the creation and the pixmap operand are
  checked.
- **GLX pbuffers own a same-XID core pixmap.** `CreatePbuffer` registers a core `Pixmap` under the pbuffer XID
  (so DRI3 can export it), and `DestroyPbuffer` frees whatever core pixmap has that XID. Both are checked as their
  own targets (review r1 B-1):
  - `CreatePbuffer` asks `CREATE` for the `GlxDrawable` and then `CREATE` for the backing `Pixmap` (as XLibre,
    `glx/glxcmds.c:1352`), both `Creation { parent: None }`, before the GLX record is inserted and before the
    backend allocation. Either denial creates nothing.
  - `DestroyPbuffer` asks `DESTROY` for the `GlxDrawable` if present (a denial answers the policy's error and
    changes nothing), then `DESTROY` for the core pixmap it is about to free, if present, whether or not a GLX
    record resolved. The pixmap is a derived target: a denial skips the free (the pixmap and its backend object
    stay) and the request still succeeds.
  - Pre-existing, unchanged with no policy: `DestroyPbuffer` frees a core pixmap of that XID even when it is
    another client's ordinary pixmap with no GLX record. Fixing that is a client-visible change and is not part of
    this plan; it is reported to the user as a finding.
- **Partial effects on denial follow today's error path at the same point.** `ChangeAlarm`: a denied counter leaves
  the earlier fields and event selection exactly as today's `BadCounter` at that point does (the clone written back on
  error). `Await` and `AwaitFence`: a denied entry installs no await, as today's missing counter or fence. Present
  `Pixmap` / `PixmapSynced` notify windows: a denied notify window answers the policy's error and presents nothing.
- **Present `QueryCapabilities`' target** is checked as a Window with `GET_ATTR` when it resolves as one, else as a
  RANDR CRTC with `GET_ATTR` when it resolves as one; an unresolved target keeps today's answer.
- **SYNC `SetPriority` / `GetPriority`'s resource** (not `None`) is checked with `GET_ATTR` through
  `validate_core_resource_if_present`, as 3b's `XISetClientPointer`; the client part is PR 5.
- **DRI3 `FreeSyncobj`** checks `WRITE` after the existing missing-syncobj `BadValue` and before the owner
  `BadAccess`, as 3b's `DeletePointerBarrier`. MIT-SHM `GetImage`'s segment is checked after the missing-segment
  `BadValue` and before the read-only `BadAccess`.

## Site table

"After X" means immediately after X's existing validation; "if present" means checked only when it resolves. Modes
are XLibre's (`include/dixaccess.h`), joined with `+`.

### SYNC (major 142)

| Request | Checks, in order |
|---|---|
| 2 CreateCounter | new counter `CREATE`, `Creation { parent: None }`, before the insert |
| 3 SetCounter, 4 ChangeCounter | counter `WRITE`, after the writable-counter helper (system `BadAccess`, missing `BadCounter`) and before ChangeCounter's overflow `BadValue` |
| 5 QueryCounter | counter `READ` (system counters included) |
| 6 DestroyCounter | counter `DESTROY`, after the writable helper, before removal |
| 7 Await | each condition's counter `READ` in the existing per-entry pass, right after that counter's lookup and before its value and test checks |
| 8 CreateAlarm | the attribute counter `READ` in the attributes helper after its lookup; new alarm `CREATE`, `Creation { parent: None }`, before the insert |
| 9 ChangeAlarm | alarm `WRITE` after `BadAlarm`, before the list-length check; attribute counter `READ` in the helper after its lookup |
| 10 QueryAlarm | alarm `READ` |
| 11 DestroyAlarm | alarm `DESTROY`, before removal |
| 12 SetPriority, 13 GetPriority | the named resource `GET_ATTR`, if present (not `None`) |
| 14 CreateFence | drawable `GET_ATTR`, if present; new fence `CREATE`, `Creation { parent }`, before the insert |
| 15 TriggerFence, 16 ResetFence | fence `WRITE`, after `BadFence`, before the backend call / ResetFence's `BadMatch` |
| 17 DestroyFence | fence `DESTROY`, before any wake or removal |
| 18 QueryFence | fence `READ` |
| 19 AwaitFence | each fence `READ` in the existing existence pass, right after its lookup |

### Present (major 145)

| Request | Checks, in order |
|---|---|
| 1 Pixmap | window `WRITE`; pixmap `READ`; then in the common helper, each after its own validation and skipping zero sentinels: valid region `READ`, update region `READ`, CRTC `READ`, wait fence `READ`, idle fence `WRITE`; then each notify window `GET_ATTR`, if present, in array order, after the common helper and before the MSC domain is selected (the selection writes `present_window_msc`) |
| 2 NotifyMSC | window `READ` |
| 3 SelectInput | window `GET_ATTR`, if present, before the event-id processing; an existing event selection `WRITE`, before its owner/window `BadMatch`; a new one `CREATE`, `Creation { parent: window }`, before the insert |
| 4 QueryCapabilities | target `GET_ATTR` as Window, else as RANDR CRTC, if present |
| 5 PixmapSynced | acquire syncobj `WRITE`, release syncobj `WRITE`, each after its own validation; then window, pixmap, valid/update regions and CRTC as row 1 (no fences); then notify windows as row 1 |

### DRI3 (major 147)

| Request | Checks, in order |
|---|---|
| 1 Open | drawable `GET_ATTR`, if present; provider `READ`, if present and not `None`; before the backend call |
| 2 PixmapFromBuffer | drawable `GET_ATTR`, if present, before the FD is taken; new pixmap `CREATE`, `Creation { parent }`, before the backend import |
| 3 BufferFromPixmap, 8 BuffersFromPixmap | pixmap `WRITE`, after the pixmap/host validation, before the export |
| 4 FenceFromFD | drawable `GET_ATTR` after `BadDrawable`; new fence `CREATE`, `Creation { parent: drawable }`, after the FD validation and before the backend import |
| 5 FDFromFence | drawable `GET_ATTR`, if present; fence `WRITE`, if present; before the backend call |
| 6 GetSupportedModifiers, 9 SetDRMDeviceInUse | window `GET_ATTR`, if present |
| 7 PixmapFromBuffers | window `GET_ATTR`, if present, before the buffer checks; new pixmap `CREATE`, `Creation { parent }`, before the backend import |
| 10 ImportSyncobj | drawable `GET_ATTR` after `BadDrawable`; new syncobj `CREATE`, `Creation { parent: drawable }`, after the FD validation and before the backend import |
| 11 FreeSyncobj | syncobj `WRITE`, after the missing `BadValue`, before the owner `BadAccess` |

### MIT-SHM (major 130)

| Request | Checks, in order |
|---|---|
| 1 Attach, 6 AttachFd, 7 CreateSegment | new segment `CREATE`, `Creation { parent: None }`, after the existing request validation (CreateSegment's size, AttachFd's FD) and before the OS attach / mapping / FD creation |
| 2 Detach | segment `READ`, if present, before removal (XLibre's mode) |
| 3 PutImage | drawable `WRITE` and GC `USE` through `validate_drawable_and_gc`'s mode arguments; segment `READ` after its `BadValue` |
| 4 GetImage | segment `READ` after its `BadValue`, before the read-only `BadAccess`; drawable `READ` after `BadDrawable`, before the readback |
| 5 CreatePixmap | drawable `GET_ATTR` after `BadDrawable`; new pixmap `CREATE`, `Creation { parent: drawable }`, after depth/size and before the backend allocation; segment `READ` after its `BadValue` (a denial here frees the allocation first) |

### GLX (major 148)

| Request | Checks, in order |
|---|---|
| 3 CreateContext, 24 CreateNewContext, 34 CreateContextAttribsARB, vendor 65541 CreateContextWithConfigSGIX (16 and 17) | share context `READ`, if present and not `None`; new context `CREATE`, `Creation { parent: None }`, before the insert |
| 4 DestroyContext | context `DESTROY`, if present, before removal |
| 5 MakeCurrent, 26 MakeContextCurrent | context `USE`, if present; draw, then read (26), per the GLX-drawable fallback ruling; before the tag is issued |
| 6 IsDirect, 25 QueryContext | context `READ`, after `GLXBadContext` |
| 11 SwapBuffers | drawable per the GLX-drawable fallback ruling |
| 13 CreateGLXPixmap | pixmap `ADD` after its validation; new GLX drawable `CREATE`, `Creation { parent: pixmap }`, before the insert |
| 15 DestroyGLXPixmap | GLX drawable `DESTROY`, after `GLXBadPixmap`, before removal |
| 22 CreatePixmap, vendor 65542 CreateGLXPixmapWithConfigSGIX (16 and 17) | pixmap `ADD` after `GLXBadPixmap`; new GLX drawable `CREATE`, `Creation { parent: pixmap }`, before the insert and export acquire |
| 23 DestroyPixmap, 32 DestroyWindow | GLX drawable `DESTROY`, if present, before the export release / removal |
| 28 DestroyPbuffer | GLX drawable `DESTROY`, if present; then the same-XID core pixmap `DESTROY`, if present, before it is freed (derived target: skipped on denial, request succeeds) |
| 27 CreatePbuffer | new GLX drawable `CREATE`, then new backing pixmap `CREATE`, both `Creation { parent: None }`, before the GLX record insert and the backend allocation |
| 29 GetDrawableAttributes | GLX drawable `GET_ATTR`, or the window `GET_ATTR` on the fallback, after the existing resolution |
| 30 ChangeDrawableAttributes | GLX drawable `SET_ATTR`, if present, before the mutable record is taken |
| 31 CreateWindow | window `ADD`, if present; new GLX drawable `CREATE`, `Creation { parent }`, before the insert |
| vendor 1330 BindTexImageEXT (16 and 17) | GLX drawable `READ`, after the membership check, before the retarget |
| vendor 1331 ReleaseTexImageEXT (16 and 17) | GLX drawable `READ`, if present |

## Review Focus

1. No-policy parity for every operand yserver does not validate today (DRI3 Open/FDFromFence/GetSupportedModifiers,
   SHM Detach, SYNC CreateFence/priority, Present notifies and SelectInput, GLX destroys/current/swap): an id that
   does not resolve, or resolves to another kind, is not asked and answers exactly as today.
2. Creations: a denied creation leaves no store entry, no backend object, no OS mapping and no backing pixmap,
   including the ordering exceptions (SHM CreatePixmap's allocation before its segment validation, freed on a later
   segment denial; GLX CreatePbuffer's record before its backend allocation). Every object a request creates or
   destroys under another kind's store (GLX pbuffer backing pixmaps) is asked under its own kind.
3. Partial effects: ChangeAlarm's written-back clone on a denied counter matches today's `BadCounter` at the same
   point; Await/AwaitFence install nothing; a denied Present notify window presents nothing.
4. Borrows: GLX ChangeDrawableAttributes checks before taking the mutable record; SHM GetImage's checks run before
   the mutable segment slice.
5. Server work stays unchecked: SYNC trigger consequences and alarm firing, Present deferred execution and
   completion, DRI3 deferred export completion, GLX export lifetime, SHM segment drop.

---

### Task 1: SYNC counters and priority

**Files:** `core_loop/process_request/sync_ext.rs`; tests in a new `process_request/tests/access_sync.rs`, registered
in `tests/mod.rs`.

**Sites:** SYNC rows 2–6, 12, 13.

- [ ] **Step 1: Failing tests:** DENY and ASK rows for every site (ASK asserts client, id, kind, mode, `Creation`
  where relevant, `target_owner` = `SERVER_OWNER` for a system counter, and `RequestOpcodes`);
  `system_counter_query_is_asked_with_server_owner`; `set_priority_unresolved_resource_is_not_asked` (no-policy row
  with today's bytes and a deny-all row that still succeeds). Mutations: check after the overflow `BadValue`; skip the
  check for system counters.
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `feat(core): access control PR 3c-i, Task 1: SYNC counters and priority`.

### Task 2: SYNC alarms and Await

**Sites:** SYNC rows 7–11.

- [ ] **Step 1: Failing tests:** DENY and ASK rows; `await_denied_second_counter_installs_nothing` (mutation: install
  per entry); `change_alarm_denied_counter_keeps_todays_partial_effects` (the same observable alarm state and event
  selection as a missing counter at that point; mutation: check the counter before the earlier fields are applied);
  `create_alarm_denied_creation_creates_nothing`.
- [ ] **Steps 2–5.** Commit `… Task 2: SYNC alarms and Await`.

### Task 3: SYNC fences

**Sites:** SYNC rows 14–19.

- [ ] **Step 1: Failing tests:** DENY and ASK rows (no backend call on a denied Trigger/Reset/Destroy);
  `await_fence_denied_entry_installs_nothing`; `create_fence_unresolved_drawable_creates_with_no_parent` (ASK records
  `parent: None`; no-policy bytes unchanged).
- [ ] **Steps 2–5.** Commit `… Task 3: SYNC fences`.

### Task 4: Present

**Files:** `core_loop/process_request/present_ext.rs`; tests `process_request/tests/access_present.rs`.

**Sites:** every Present row.

- [ ] **Step 1: Failing tests:** DENY and ASK rows, the common-helper operands each with a zero-sentinel row that
  asks nothing; `present_pixmap_denied_notify_window_presents_nothing` (no queued present, no backend copy, no
  completion, and the window's remembered MSC domain (`present_window_msc`) unchanged; mutation: check notifies after
  the domain selection); mixed-failure rows for both Pixmap and PixmapSynced (review r2 M-2), each paired with its
  no-policy result: a denied earlier operand with an invalid later one (denied valid region + missing update region;
  denied window + missing pixmap; PixmapSynced: denied acquire syncobj + invalid release syncobj), and a denied last
  resource with invalid options or remainder (denied CRTC, and for Pixmap denied idle fence, + invalid options); the
  policy's error answers each. Mutation: run every check after the complete validity pass; `present_select_input_window_checked_before_bad_match`;
  `query_capabilities_crtc_target_is_asked_with_server_owner`.
- [ ] **Steps 2–5.** Commit `… Task 4: Present`.

### Task 5: DRI3

**Files:** `core_loop/process_request/dri3.rs`; tests `process_request/tests/access_dri3.rs`.

**Sites:** every DRI3 row.

- [ ] **Step 1: Failing tests:** DENY and ASK rows; denied creations make no backend import and register nothing (the
  received FD is closed as on today's error paths); `free_syncobj_denied_before_owner_bad_access` (another client's
  syncobj with a deny policy answers the policy's error; no-policy row keeps `BadAccess`);
  `buffer_from_pixmap_denied_parks_no_export`.
- [ ] **Steps 2–5.** Commit `… Task 5: DRI3`.

### Task 6: MIT-SHM

**Files:** `core_loop/process_request/xshm.rs`; tests `process_request/tests/access_shm.rs`.

**Sites:** every MIT-SHM row.

- [ ] **Step 1: Failing tests:** DENY and ASK rows; `shm_create_pixmap_denied_creation_allocates_nothing` (no backend
  allocation; mutation: ask after the allocation); `shm_create_pixmap_denied_segment_frees_allocation` (creation
  allowed, segment denied: the backend log shows the allocation and its free, no pixmap is registered; mutation:
  return without freeing); `shm_get_image_denied_drawable_reads_back_nothing`;
  `shm_detach_unresolved_segment_is_not_asked`; denied Attach/AttachFd/CreateSegment create no mapping and send no FD.
- [ ] **Steps 2–5.** Commit `… Task 6: MIT-SHM`.

### Task 7: GLX contexts and current state

**Files:** `core_loop/process_request/glx.rs`; tests `process_request/tests/access_glx.rs`.

**Sites:** GLX rows 3, 4, 5, 6, 11, 24, 25, 26, 34 and vendor 65541 (both envelopes).

- [ ] **Step 1: Failing tests:** DENY and ASK rows; `make_current_draw_falls_back_to_core_window` (a core window that
  is no GLX drawable is asked `GET_ATTR`; a GLX window drawable is asked `WRITE`); `make_current_unresolved_ids_are_not_asked`;
  `create_context_denied_share_creates_nothing`; the unsupported GLX requests listed in Scope decisions answer
  exactly as today with a deny-all policy and record no check.
- [ ] **Steps 2–5.** Commit `… Task 7: GLX contexts and current state`.

### Task 8: GLX drawables

**Sites:** GLX rows 13, 15, 22, 23, 27–32 and vendor 65542, 1330, 1331 (both envelopes).

- [ ] **Step 1: Failing tests:** DENY and ASK rows; `create_pbuffer_denied_creates_no_record_and_no_backing`
  (a policy denying only `Pixmap` `CREATE` and one denying only `GlxDrawable` `CREATE`: neither creates a record,
  a core pixmap or a backend allocation; mutation: ask after the record insert; drop the pixmap check);
  `destroy_pbuffer_denied_core_pixmap_is_kept` (B's DestroyPbuffer naming A's bare core pixmap with a policy denying
  `Pixmap` `DESTROY`: A's pixmap and backend object remain, the request succeeds; with no policy it is freed as
  today; mutation: ask only when a GLX record resolves); `change_drawable_attributes_denied_keeps_event_mask`;
  `bind_tex_image_denied_does_not_retarget`; destroy rows with an id that is no GLX drawable answer as today.
- [ ] **Steps 2–5.** Commit `… Task 8: GLX drawables`.

### Task 9: Spec rows, status and gate

- [ ] **Step 1:** spec, "A denial is interpreted by its site": rows for `ChangeAlarm` (a denied counter leaves the
  fields applied before it, as a missing counter does) and Present notify windows (the policy's error; nothing
  presented). "What a policy can see": server objects (system SYNC counters, RANDR CRTCs and providers) are asked
  with owner `SERVER_OWNER`; GLX context tags, screens, visuals and FBConfigs are not resources.
- [ ] **Step 2:** `docs/status.md`: one entry at the top for PR 3c-i.
- [ ] **Step 3: Gate:** `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`, also with
  `--features tcp-transport` and `--features xdmcp`; FreeBSD clippy; `cargo check -p yserver-core --target
  x86_64-pc-windows-gnu`; `cargo test --workspace` and with `--features xdmcp`; each `crates/*/tests` file;
  `--features xdmcp -- --ignored` (lavapipe); the access test modules 5 runs under `nproc` busy loops;
  `tools/check-layout.sh`; `just man`.
- [ ] **Step 4: Commit** `docs: access control PR 3c-i spec rows and status`.
