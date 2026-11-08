# Access control layer, PR 3b — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resource checks at every request of RENDER, SHAPE, XFIXES, Composite and DAMAGE, and at the window,
cursor and barrier operands of XInput, XKB and XTEST, with no change any client can observe while no policy is
installed.

**Architecture:** New check sites, plus one addition under them: the identity of a cursor that is still displayed after its XID was freed (Task 5). PR 3a (#342, merged as 2ed4e5b9) provides everything they call:
`ServerState::check_resource`, `AccessContext::owner` for every `ResourceKind`, `RequestOpcodes::for_request`, the
`validate_resource_access` / `validate_resource_if_present` helpers in `core_loop/process_request.rs`, and every
`AccessMode` bit these sites use. Each site asks right after its operand's existing validation, in yserver's order.

**Tech Stack:** Rust, crate `yserver-core`; std only (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, series row 3. Plan 3a
(`docs/superpowers/plans/2026-11-05-access-control-pr3a.md`) is the first part of the same row.

**Source inventory:** `~/Projects/claude-scratch/access-control/pr3/inventory.md` §2 (RENDER, SHAPE, XFIXES,
Composite and DAMAGE, XInput, XKB, Other extensions) and §3, taken at `8c99a1e3`. The implementer locates sites by
function name; line numbers drift.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence. The existing
  suite passes unmodified.
- No new validity errors: an operand yserver does not validate today is checked only when its id resolves to an
  object of the expected kind (`validate_resource_if_present`), and an id that does not resolve keeps today's
  behaviour exactly.
- Reuse first: every site calls 3a's helpers; no second resolver, no second opcode constructor, no new
  `AccessMode` bit unless a row below names it (none does). `verify_pictures!` / `first_missing_picture` gain the
  Picture check instead of each RENDER handler.
- No environment variable, no `cfg(test)` inside production function bodies. Test policies live in test modules
  and reuse 3a's shared recording and decision path (`tests/extensions.rs`, `ExtensionHook` / `check`).
- Device operands (XI, XKB, XTEST) are PR 5. Send checks (`SendExtensionEvent`) are PR 4. The selection hook
  (XFIXES `SelectSelectionInput`'s selection) stays deferred, and the screen hook is not part of PR 3.
- Licensing: Xorg and XLibre are MIT and may be read; no copyleft source is consulted. No code is copied.
- Tests exercise production paths through `process_request`. Every new test names the mutation that makes it fail.
  Suites run under CPU load.

## Scope decisions

Plan 3a's "Scope decisions" apply unchanged: the policy's error goes out verbatim; every derived target gets its
own check and a derived-target denial never produces an error; `SERVER_OWNER` work is not checked; creations ask
with `CREATE` and a `Creation` record after every existing validation and before the first allocation; modes are
XLibre's at each site; policies see an extension request's minor opcode from the data byte. The denial test
contract of plan 3a ("Denial test contract") applies to every row here. Rulings new to this plan:

- **PictFormat and other advertised descriptors are not checked.** XLibre asks `Read` on a RENDER PictFormat
  (CreatePicture, CreateGlyphSet, the mask formats). In yserver formats are a static server table with no owner and
  no `ResourceKind`; a policy cannot tell one client's use of a format from another's. Not checked, as glyph
  indices and atoms are not.
- **Derived targets in 3b**, each checked with the mode the row names; a denied one is skipped and the request
  succeeds (plan 3a's rule):
  - `RedirectSubwindows` / `UnredirectSubwindows`: each current child gets the parent's modes. A skipped child keeps
    its current redirect state. The persistent rule for future children is the parent's and is not per child.
  - `DAMAGE Add`: each Damage object on the drawable that the added region would accumulate into gets `WRITE`. A
    skipped Damage does not accumulate. Notifying a Damage's owner is delivery, a receive check (PR 4).
  - `ChangeCursorByName`: each cursor whose name matches gets `WRITE+SET_ATTR`, as `ChangeCursor`'s destination. A
    skipped cursor keeps its image.
  - Windows, grabs and aliases that merely reference a changed cursor are consequences, not targets (the cursor
    object changes; they keep the same reference).
- **A client value never reaches the backend as a backend handle (bug fix first, Task 0).** Two RENDER paths
  forward client XIDs where the renderer expects its own handles, which are allocated separately
  (`render_ops.rs` `render_create_glyphset` uses `next_host_xid()`):
  - CompositeGlyphs carries a client GlyphSet XID in each switch. The core resolves a looked-up switch as a
    GlyphSet before translating it to the renderer's handle; a switch that does not name a client GlyphSet
    returns `BadGlyphSet` and the request draws nothing. Xorg does not look up a switch whose XID ends exactly
    at the end of the stream (`render/render.c`).
  - CreatePicture forwards its value list unchanged (`render.rs`, `render_create_picture(.., req.values)`), so the
    alpha map and clip mask reach the backend as client XIDs; ChangePicture already translates them
    (`change_picture_translate_xids`).
  Task 0 translates each valid switch value from a client GlyphSet XID to its backend handle before forwarding;
  PR 3b performs the switch's `USE` check in that same pass, after the earlier operands' checks. CreatePicture
  shares ChangePicture's per-value translation but keeps its own outcome for a value that
  does not resolve: it forwards 0 for that attribute (no alpha map, no clip mask) and registers the picture as it
  does today, since CreatePicture's registry is authoritative once no error was sent and the raw client XID it
  forwards today already selects nothing. A looked-up unknown CompositeGlyphs switch returns `BadGlyphSet` as #347
  requires; the access layer checks `USE` in the same stream-order pass that validates and translates each switch.
  ChangePicture keeps its historical rule unchanged: an unresolved
  attribute drops the whole request (`c00ddeb2`, Phase 3.5 spec, "dropping missing translations"). This changes client-visible behaviour the way Xorg
  behaves (switches take effect; a backend handle is no longer accepted).
- **Retained cursor identity.** A cursor's XID can be freed while a window still displays it: `free_cursor` drops the
  table entry and keeps the host cursor (`resources/picture_font_cursor.rs`), and its name keeps matching
  (`d746bc3c`, `docs/status.md:720-723`: "a freed-but-displayed cursor still matches"). Such a cursor is still a
  client's object, so it keeps an identity: the `ResourceTable` records, for each host cursor, the XID and owner it
  was first bound under (the XID whose creation produced it; a later `ChangeCursor` alias does not replace it). The
  record remains while the core references the host or the backend reports it alive; existing release points sweep
  stale records, with no periodic work. KMS host XIDs are monotonic and never reused. The owner belongs to that host-cursor
  instance, not to the number: after its owner disconnects, the XID base is recycled (`8677f535`, `d5609288`) and
  another client can create a live cursor with the same XID while the retained one is still displayed. So a check on
  a retained instance carries its recorded owner in the target: `ResourceTarget` gains `owner: Option<ClientId>`,
  `None` at every existing site (the object is live and the resolver answers), set from the record for a retained
  host cursor. `AccessContext` gains `target_owner(&ResourceTarget) -> Option<ClientId>`, which answers
  `target.owner` when set and `owner(id, kind)` otherwise; policies use it. `AccessContext::owner` itself stays a
  resolver of live objects only. Host cursors with no record (server cursors: none, the root default) are not
  checked.
- **`GetCursorImage` and `GetCursorImageAndName`** check the displayed cursor, which they derive, with `READ`
  (`+GET_ATTR` for the name), as XLibre, against the recorded XID of the displayed host cursor. A displayed host cursor
  with no record is a server cursor and is not checked.
- **`ChangeCursorByName`'s derived cursors** are today's match set unchanged: every host cursor with that name,
  retained ones included. Each one with a record is checked against its recorded XID; one without a record is not.
- **XInput `XISetClientPointer` / `XIGetClientPointer`** name an arbitrary resource whose owner is the target
  client; yserver ignores the body today. The resource part is checked only when the id resolves through
  `resource_owner`'s stores, exactly as 3a's `KillClient` row: `XISetClientPointer` with `MANAGE`,
  `XIGetClientPointer` with `GET_ATTR` (XLibre); the client and device parts are PR 5.
- **`XIBarrierReleasePointer`** keeps XLibre's sequential shape: each entry's barrier is checked (`READ`) when the
  loop reaches it, and a denial reports the policy's error after the earlier entries took effect, as XLibre
  preserves earlier releases (`Xi/xibarriers.c:921-945`). This is a whole-request denial with partial effects, the
  same as today's `BadValue` for a later missing barrier.

## Site table

"After X" means immediately after X's existing validation; with no validation today the "no new validity errors"
constraint applies. Modes are XLibre's (`include/dixaccess.h`), joined with `+`.

### RENDER (major 133)

| Request | Checks, in yserver's order |
|---|---|
| 4 CreatePicture | drawable `READ+ADD`; alpha map picture `READ`; clip mask pixmap `READ`; new picture `CREATE+SET_ATTR`, `Creation { parent: drawable }` |
| 5 ChangePicture | picture `SET_ATTR`; alpha map `READ`, clip mask `READ`, during the existing value-list translation, before any change is forwarded |
| 6 SetPictureClipRectangles, 28 SetPictureTransform, 30 SetPictureFilter | picture `SET_ATTR` |
| 7 FreePicture | picture `DESTROY`, before removal |
| 8 Composite | dst `WRITE`, src `READ`, mask `READ`, in yserver's order (dst, src, mask) |
| 10–13 Trapezoids, Triangles, TriStrip, TriFan | src `READ`, dst `WRITE` |
| 17 CreateGlyphSet | new glyphset `CREATE`, `Creation { parent: None }`, before the backend call |
| 18 ReferenceGlyphSet | existing glyphset `GET_ATTR`; new alias `CREATE`, `Creation { parent: None }` |
| 19 FreeGlyphSet | glyphset `DESTROY`, before removal |
| 20 AddGlyphs | glyphset `ADD` |
| 22 FreeGlyphs | glyphset `REMOVE` |
| 23–25 CompositeGlyphs8/16/32 | src `READ`, dst `WRITE`, initial glyphset `USE`, then each looked-up stream switch is resolved as a client GlyphSet and checked for `USE` in stream order in the same translation pass; an unknown switch returns `BadGlyphSet`; all before the stream reaches the backend |
| 26 FillRectangles | dst `WRITE` |
| 27 CreateCursor | source picture `READ`; new cursor `CREATE`, `Creation { parent: None }` |
| 29 QueryFilters | drawable `GET_ATTR` |
| 31 CreateAnimCursor | each frame cursor `READ` in the existing per-frame pass; new cursor `CREATE`, `Creation { parent: None }` |
| 32 AddTraps | picture `WRITE` |
| 33–36 CreateSolidFill, CreateLinearGradient, CreateRadialGradient, CreateConicalGradient | new picture `CREATE`, `Creation { parent: None }` |

Not checked: QueryPictIndexValues (format only), QueryDithers, Scale, ColorTrapezoids, ColorTriangles,
AddGlyphsFromPicture (unsupported stubs that read no operand).

### SHAPE (major 141)

| Request | Checks |
|---|---|
| 1 Rectangles, 4 Offset | dest window `SET_ATTR` |
| 2 Mask | dest window `SET_ATTR`; source pixmap `READ` |
| 3 Combine | dest window `SET_ATTR`; source window `GET_ATTR`, in yserver's order (source is read first) |
| 5 QueryExtents, 7 InputSelected, 8 GetRectangles | window `GET_ATTR` |
| 6 SelectInput | window `RECEIVE` |

### XFIXES (major 140)

| Request | Checks |
|---|---|
| 1 ChangeSaveSet | window `MANAGE`, before the mutable `ClientState` borrow |
| 2 SelectSelectionInput, 3 SelectCursorInput, 29 HideCursor, 30 ShowCursor | window `GET_ATTR` |
| 4 GetCursorImage | current cursor `READ` (scope decision) |
| 25 GetCursorImageAndName | current cursor `READ+GET_ATTR` |
| 5 CreateRegion | new region `CREATE`, `Creation { parent: None }` |
| 6 CreateRegionFromBitmap | bitmap pixmap `READ`; new region `CREATE`, `Creation { parent: bitmap }` |
| 7 CreateRegionFromWindow | window `GET_ATTR`; new region `CREATE`, `Creation { parent: window }` |
| 8 CreateRegionFromGC | GC `GET_ATTR`; new region `CREATE`, `Creation { parent: None }` |
| 9 CreateRegionFromPicture | picture `GET_ATTR`; new region `CREATE`, `Creation { parent: None }` |
| 10 DestroyRegion | region `WRITE`, as XLibre (`xfixes/region.c:245`), before removal |
| 11 SetRegion, 17 TranslateRegion | region `WRITE` |
| 12 CopyRegion, 18 RegionExtents, 28 ExpandRegion | source `READ`, destination `WRITE` |
| 13–15 Union/Intersect/SubtractRegion | source 1 `READ`, source 2 `READ`, destination `WRITE` |
| 16 InvertRegion | source `READ`, destination `WRITE` |
| 19 FetchRegion | region `READ` |
| 20 SetGCClipRegion | GC `SET_ATTR`; region `READ` |
| 21 SetWindowShapeRegion | window `SET_ATTR`; region `WRITE`, as XLibre (`xfixes/region.c:499`) |
| 22 SetPictureClipRegion | picture `SET_ATTR`; region `READ` |
| 23 SetCursorName | cursor `SET_ATTR` |
| 24 GetCursorName | cursor `GET_ATTR` |
| 26 ChangeCursor | source `READ+GET_ATTR`; destination `WRITE+SET_ATTR` |
| 27 ChangeCursorByName | source `READ+GET_ATTR`; each matching cursor `WRITE+SET_ATTR` (derived) |
| 31 CreatePointerBarrier | window `READ`; new barrier `CREATE`, `Creation { parent: window }` |
| 32 DeletePointerBarrier | barrier `DESTROY`, after the existing missing-barrier `BadValue` and before the owner `BadAccess` |

A destination region that does not exist keeps today's behaviour (written as new and owned by the requester): the
check runs only when it resolves.

### Composite (major 144) and DAMAGE (major 143)

| Request | Checks |
|---|---|
| 144/1 RedirectWindow, 144/3 UnredirectWindow | window `SET_ATTR+MANAGE+BLEND` |
| 144/2 RedirectSubwindows, 144/4 UnredirectSubwindows | parent `SET_ATTR+MANAGE+BLEND`; each current child the same (derived) |
| 144/5 CreateRegionFromBorderClip | window `GET_ATTR`; new region `CREATE`, `Creation { parent: window }` |
| 144/6 NameWindowPixmap | window `GET_ATTR`; new pixmap `CREATE`, `Creation { parent: window }` |
| 144/7 GetOverlayWindow | the supplied window `GET_ATTR` (it resolves only when valid; today it is ignored) |
| 144/8 ReleaseOverlayWindow | the supplied window `GET_ATTR`, same rule |
| 143/1 Create | drawable `GET_ATTR+READ`; new damage `CREATE`, `Creation { parent: drawable }` |
| 143/2 Destroy | damage `DESTROY`, before removal |
| 143/3 Subtract | damage `WRITE`; repair region `WRITE`; parts region `WRITE` |
| 143/4 Add | drawable `WRITE`, region `WRITE`, in yserver's order; each accumulating Damage `WRITE` (derived) |

### XInput (major 137), XKB (major 136), XTEST (major 146): window, cursor and barrier operands

| Request | Checks (device parts are PR 5) |
|---|---|
| 6 SelectExtensionEvent, 46 XISelectEvents | window `RECEIVE` |
| 7 GetSelectedExtensionEvents, 9 GetDeviceDontPropagateList, 40 XIQueryPointer, 60 XIGetSelectedEvents | window `GET_ATTR` |
| 8 ChangeDeviceDontPropagateList | window `SET_ATTR` |
| 13 GrabDevice, 15 GrabDeviceKey, 16 UngrabDeviceKey, 17 GrabDeviceButton, 18 UngrabDeviceButton, 55 XIPassiveUngrabDevice | grab window `SET_ATTR` |
| 21 SetDeviceFocus, 49 XISetFocus | focus window `SET_ATTR` |
| 41 XIWarpPointer | dst `GET_ATTR`, then src `GET_ATTR` |
| 42 XIChangeCursor | window `SET_ATTR`; cursor `USE` |
| 44 XISetClientPointer | named resource `MANAGE` |
| 45 XIGetClientPointer | named resource `GET_ATTR` |
| 51 XIGrabDevice, 54 XIPassiveGrabDevice | grab window `SET_ATTR`; cursor `USE` |
| 61 XIBarrierReleasePointer | each barrier `READ` in the existing loop (scope decision) |
| 136/3 XKB Bell | window `GET_ATTR` |
| 146/1 XTEST CompareCursor | window `GET_ATTR`; cursor `READ` |
| 146/2 XTEST FakeInput | the event's root window `GET_ATTR` |

## Review Focus

0. Task 0's translation: every path that forwards a client value the renderer uses as a key (glyphset switches,
   CreatePicture and ChangePicture attributes) goes through a translation, and none accepts a backend handle.
1. `verify_pictures!`: every RENDER request that uses it asks for each picture with the row's mode, in argument
   order, and a picture-less (sourceless) drawable path does not skip the check.
2. Regions written by one client and owned by another (Task 1 of 3a kept the owner): a denied `WRITE` leaves the
   other client's region and its owner unchanged.
3. Derived targets (`RedirectSubwindows`, `DAMAGE Add`, `ChangeCursorByName`): a skipped one is exactly unchanged
   and the request still succeeds and acts on the rest.
4. `XIBarrierReleasePointer`'s partial effects on denial match today's partial effects on a missing barrier.
5. The retained cursor record's lifetime: created once, never by an alias, kept while the core references the host
   or the backend reports it alive, then removed at a later existing release point (XID free, window cursor change,
   ChangeCursor retarget, disconnect, reset).
6. Stubs and ignored bodies (`GetOverlayWindow`'s window, `XISetClientPointer`): with no policy nothing they answer
   changes; with a policy, an id that does not resolve is not asked.

---

### Task 0: RENDER client values are translated to backend handles

A bug fix, separate from the access layer, committed first because Tasks 1 and 2 check the client XIDs this
translation resolves.

**Files:** `core_loop/process_request/render.rs` (CreatePicture; CompositeGlyphs8/16/32); tests in the existing
RENDER test file under `process_request/tests`.

**Invariants:** no client-supplied value reaches a backend lookup without translation; a backend handle value from a
client never selects a backend object; a looked-up switch that is not a client GlyphSet returns `BadGlyphSet`.
Switch existence and the PR 3b `USE` check are evaluated together in stream order, preserving Xorg's end-of-stream
lookup boundary. One translation helper per value kind, shared by the sites that need it (reuse
`change_picture_translate_xids`).

- [ ] **Step 1: Failing tests** through `process_request` with a backend whose handles differ from the client XIDs
  (`RecordingBackend` records the arguments it receives):
  - `composite_glyphs_switch_is_translated_to_backend_handle`: a stream that switches from glyphset G1 to G2 reaches
    the backend with G2's backend handle in the switch item. Mutation: forward the item bytes unchanged.
  - `composite_glyphs_switch_to_non_glyphset_is_bad_glyphset`: a switch whose value equals another client's
    backend handle (and is not a client GlyphSet XID) reports `BadGlyphSet` before forwarding or drawing.
  - `create_picture_alpha_map_and_clip_mask_are_translated`: the backend receives the alpha map picture's and the
    clip mask pixmap's backend handles. Mutation: pass `req.values` unchanged.
  - `create_picture_unresolved_alpha_map_still_registers_picture`: CreatePicture with a valid drawable and an alpha
    map value that is no picture: the backend receives 0 for it, the picture exists, and a later
    SetPictureTransform on it succeeds; scalar attributes in the same list (repeat) still apply. Mutation: drop the
    request as ChangePicture does.
  - `change_picture_unresolved_alpha_map_still_drops_request`: ChangePicture's existing drop of the whole request,
    scalar attributes included, is unchanged. Mutation: forward 0 as CreatePicture does.
  - the existing Stage 3d inline-switch test keeps passing (both glyphsets' atlas entries read).
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `fix(render): translate CompositeGlyphs switches and CreatePicture's alpha map and clip mask to backend handles`.

### Task 1: RENDER pictures and drawing

**Files:** `core_loop/process_request/render.rs`; tests `process_request/tests/access_render.rs` (new, registered in
`tests/mod.rs`).

**Sites:** RENDER rows 4–13, 26, 28–30, 32–36.

- [ ] **Step 1: Failing tests:**
  - `render_picture_requests_deny_before_effects` (table, one row per request and per operand): the policy's error
    with major 133 and the request's minor; no backend call; pictures unchanged (attributes, transform, filter, clip),
    and no picture exists after a denied creation.
  - `render_picture_requests_ask_expected_targets` (recording): client, id, kind, mode, `Creation` record, opcodes,
    order (Composite: dst, src, mask).
  - `change_picture_denied_alpha_map_changes_nothing`: a ChangePicture setting repeat, then an alpha map that is
    denied: no attribute changed. Mutation: forward each attribute as translated.
  - `composite_denied_mask_after_valid_dst_and_src`: precedence with a missing mask (BadPicture, no policy) and a
    denied mask (policy error).
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `feat(core): access control PR 3b, Task 1: RENDER pictures and drawing`.

### Task 2: RENDER glyphsets and cursors

**Sites:** RENDER rows 17–20, 22–25, 27, 31.

- [ ] **Step 1: Failing tests:** deny and ask tables for these rows;
  `composite_glyphs_denied_stream_switch_draws_nothing` (a stream that switches to a denied glyphset after
  glyphs from an allowed one: no backend draw, policy error; mutation: check each switch when the stream reaches it
  after drawing); `create_anim_cursor_denied_frame_creates_nothing`.
- [ ] **Steps 2–5.** Commit `… Task 2: RENDER glyphsets and cursors`.

### Task 3: SHAPE

**Sites:** SHAPE rows 1–8.

- [ ] **Step 1: Failing tests:** deny and ask tables; every row also has a no-policy case with an id that does not
  resolve, asserting today's bytes (SHAPE validates none of its windows today).
- [ ] **Steps 2–5.** Commit `… Task 3: SHAPE`.

### Task 4: XFIXES regions

**Sites:** XFIXES rows 5–22, 28, and Composite 144/5.

- [ ] **Step 1: Failing tests:** deny and ask tables;
  `region_write_into_other_clients_region_denied_keeps_it` (client B's CopyRegion into A's region with `WRITE`
  denied: A's rectangles and owner unchanged); `missing_destination_region_is_not_asked` (CopyRegion into an
  unused id with a deny-all policy: today's bytes, the new region exists owned by the requester).
- [ ] **Steps 2–5.** Commit `… Task 4: XFIXES regions`.

### Task 5: Retained cursor identity

**Files:** `access.rs` (`ResourceTarget::owner`, `AccessContext::target_owner`), `resources/picture_font_cursor.rs`,
`resources/mod.rs` (the record beside `cursor_host_names`), every existing `ResourceTarget` construction (adds
`owner: None`), and the host-cursor release paths (`free_cursor`, `cursor_host_released`,
`take_unreferenced_cursor_hosts` and their callers); tests in `process_request/tests/access_resources.rs` or a new
`access_cursors.rs`.

**Interfaces:** Produces `ResourceTable::cursor_host_origin(host: u32) -> Option<(ResourceId, ClientId)>`,
`ResourceTarget::owner: Option<ClientId>` (every existing construction passes `None`) and
`AccessContext::target_owner(&ResourceTarget) -> Option<ClientId>`, as described in Scope decisions. Consumed by
Task 6.

**Invariants:** a host cursor has a record from the moment it is first bound to a client cursor XID while either the
core references it or the backend reports it alive; existing release points remove it once neither does. An alias
(`retarget_cursor_host`) never creates or overwrites a record; `resource_owner` and `KillClient` are unchanged.

- [ ] **Step 1: Failing tests** (through `process_request`):
  - `freed_displayed_cursor_record_lives_until_release`: A creates cursor C, sets it on a window, frees C: the host
    cursor's record names (C, A). After the window's cursor changes and the host cursor is released, the record is
    gone. Mutation: drop the record in `free_cursor`; keep it after release.
  - `recycled_base_cursor_keeps_its_owner`: A's cursor C is displayed on B's window; A disconnects; client D gets A's
    recycled base and creates a live cursor with XID C. A check on the retained instance presents
    `target_owner` = A while `ctx.owner(C, Cursor)` answers D for the live one. With two retained host cursors
    sharing the number C (one from A, one from D after D frees its own while displayed), each check presents its
    own owner. Mutation: resolve the retained instance's owner through `ctx.owner`.
  - `change_cursor_alias_keeps_creator_record`: XFIXES ChangeCursor makes B's cursor share A's host cursor; the record
    of that host cursor still names A's XID.
- [ ] **Steps 2–5.** Commit `… Task 5: retained cursor identity`.

### Task 6: XFIXES cursors, windows and barriers

**Sites:** XFIXES rows 1–4, 23–27, 29–32.

- [ ] **Step 1: Failing tests:** deny and ask tables; `change_cursor_by_name_skips_denied_cursor` (two cursors
  named "x", one denied: the allowed one takes the source image, the denied one keeps its own, no error);
  `get_cursor_image_server_cursor_is_not_asked`; `xfixes_change_save_set_denied_leaves_save_set_unchanged`;
  for a named, displayed cursor after `FreeCursor` (Task 5's record): `get_cursor_image_of_freed_displayed_cursor_denied`
  (B's GetCursorImage and GetCursorImageAndName answer the policy's error, no image bytes),
  `change_cursor_by_name_freed_displayed_cursor_denied_keeps_image`, and
  `change_cursor_by_name_freed_displayed_cursor_no_policy_matches_today` (with no policy the replacement happens
  exactly as today). Mutation: skip the check when the XID is not live. Plus, through the recycled-base setup of
  Task 5: D's GetCursorImage of A's retained cursor is asked with owner A, and a policy allowing only the
  requester's own resources denies it.
- [ ] **Steps 2–5.** Commit `… Task 6: XFIXES cursors, windows and barriers`.

### Task 7: Composite and DAMAGE

**Sites:** Composite rows 144/1–4, 6–8; DAMAGE rows 143/1–4.

- [ ] **Step 1: Failing tests:** deny and ask tables; `redirect_subwindows_skips_denied_child` (three children, the
  middle one denied: the others redirected with backings, the middle one not, no error; the future-child rule is
  still recorded); `damage_add_skips_denied_damage` (two Damage objects on the drawable, one denied: only the other
  accumulates, no error); `damage_subtract_denied_parts_writes_nothing`.
- [ ] **Steps 2–5.** Commit `… Task 7: Composite and DAMAGE`.

### Task 8: XInput, XKB and XTEST window, cursor and barrier operands

**Files:** `core_loop/process_request/xi/dispatch/*.rs`, `xi/mod.rs`, `focus_pointer.rs` (XIWarpPointer),
`xkb.rs`, `xtest.rs`; tests `process_request/tests/access_input.rs`.

**Sites:** every row of the XInput/XKB/XTEST table.

- [ ] **Step 1: Failing tests:** deny and ask tables (device operands valid, so only the resource checks decide);
  `barrier_release_denied_second_entry_keeps_first_release` (mutation: check all barriers before releasing any);
  `xi_set_client_pointer_unresolved_id_is_not_asked`.
- [ ] **Steps 2–5.** Commit `… Task 8: XInput, XKB and XTEST operands`.

### Task 9: Spec rows and gate

- [ ] **Step 1:** spec, "A denial is interpreted by its site": rows for the 3b derived targets (`RedirectSubwindows`
  / `UnredirectSubwindows` children, `DAMAGE Add` Damage objects, `ChangeCursorByName` cursors: skipped, the request
  succeeds) and for `XIBarrierReleasePointer` (policy error after earlier entries took effect). "What a policy can
  see": state that advertised descriptors (PictFormat) are not checked, and that a cursor still displayed after its
  XID was freed keeps its creator's XID and owner until the host cursor is released.
- [ ] **Step 2: Gate:** `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`, also with
  `--features tcp-transport` and `--features xdmcp`; FreeBSD clippy; `cargo check -p yserver-core --target
  x86_64-pc-windows-gnu`; `cargo test --workspace` and with `--features xdmcp`; each `crates/*/tests` file;
  `--features xdmcp -- --ignored` (lavapipe); the access test modules 5 runs under `nproc` busy loops; `just man`.
- [ ] **Step 3: Commit** `docs(spec): access control PR 3b denial rows`.
