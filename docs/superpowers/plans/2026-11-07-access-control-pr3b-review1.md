# Access control PR 3b — maintainer review round 1

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address joske's review of #343: compact the access test rows, fix six check orders and XISetClientPointer's mode, and keep a retained cursor's record until the backend destroys the cursor.

**Architecture:** Each fix stays in the handler or resource method that owns it; the retained-cursor fix adds one backend query so the core keeps the record exactly as long as the backend keeps the cursor, instead of re-deriving the backend's references in the core.

**Tech Stack:** Rust; `yserver-core` (`core_loop::process_request`, `resources`, `backend` trait, `RecordingBackend`); `yserver` KMS backend (`kms/render/backend/portable/cursor.rs`).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`; base plan `docs/superpowers/plans/2026-11-06-access-control-pr3b.md`. Reference (MIT, behaviour and code may be read): `/data/Projects/xlibre`.

## Global Constraints

- Everything in `~/Projects/claude-scratch/access-control/sdd-pr3b/codex-common.md` binds every task (shared test policy path in `tests/extensions.rs`; a DENY and an ASK row per site; checks right after each operand's own validation; `SERVER_OWNER` unchecked; no new validity errors).
- With no policy installed, nothing a client can observe changes (errors, values, replies, events, backend calls, damage): every task here is policy-only or test-only.
- Modes and check order are XLibre's at each site.
- `tools/check-layout.sh` must pass: test files are limited to 6000 lines.
- Reuse first: new production code only where the goal is impossible otherwise, each piece justified in the report.
- Every new or changed test goes through `process_request` (or the real KMS backend for backend behaviour), and names in a comment the mutation that makes it fail.

## Review Focus

1. A retained cursor that is shown again after its record is gone (grab → free → ungrab → regrab with another cursor) must never resolve to a stale owner.
2. Freeing a cursor that nothing displays must still drop its record (no unbounded growth over a session that creates cursors continuously).
3. The nested (host X11) backend must keep today's behaviour exactly.
4. A check-order fix must not move the operand's own validation error: with no policy, the first invalid operand still reports exactly as today.
5. Compacted rows must assert exactly what the long rows asserted (same targets, modes, owners, opcodes, error values, no-effect checks).

---

### Task 1: Compact the access test rows

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/access_*.rs` (all seven), `tests/extensions.rs` only if a shared helper belongs there.

**Interfaces:**
- Produces: the compact row form Tasks 2 and 3 add rows in.

- [ ] **Step 1:** Record `wc -l` of every `access_*.rs` file in the report.
- [ ] **Step 2:** Rewrite the per-site rows (the `*Case` structs' constructions and the per-site DENY/ASK test bodies) so a row is one constructor call or table entry that rustfmt keeps to at most 3 lines, through helpers shared by the file (or by `tests/extensions.rs` when two files need them). Rows keep every assertion they have today; the change is form only.
- [ ] **Step 3:** If `access_resources.rs` is still above 5000 lines after Step 2, move whole test groups by area into a new `access_*.rs` file registered in `tests/mod.rs`, so every access file ends at or below 5000 lines.
- [ ] **Step 4:** Verify no assertion was lost: list every per-site DENY and ASK row (request name + row kind) before and after, in the report, and they match one to one; each shared helper is reviewed to assert the same targets, modes, owners, opcodes, error values and no-effect checks as the rows it replaces; the number of `#[test]` functions per file (or across moved files) is unchanged; and `cargo test -p yserver-core --lib -- core_loop::process_request::tests::access_` passes with the same test count as before Step 2. Mutation check: revert one site's check in production (e.g. remove `check_resource!` from RENDER `FreePicture`) — the same tests fail before and after compaction.
- [ ] **Step 5:** `tools/check-layout.sh` passes. Report the new line counts.

### Task 2: Check order and XISetClientPointer's mode

**Files:**
- Modify: `process_request/shape_xfixes.rs` (ExpandRegion, SelectSelectionInput, SelectCursorInput), `process_request/xtest.rs` (CompareCursor), `process_request/focus_pointer.rs` (XIWarpPointer), `process_request/composite_damage.rs` (Redirect/Unredirect Window/Subwindows), `process_request/render.rs` (CreatePicture), `process_request/xi/dispatch/devices.rs` (XISetClientPointer).
- Test: the existing access test file of each extension, in Task 1's row form.

**Interfaces:**
- Consumes: Task 1's row helpers.

What must hold (policy installed; XLibre file in brackets):

| Request | Order that must hold |
|---|---|
| XFIXES ExpandRegion [`Xext/xfixes/region.c` `ProcXFixesExpandRegion`] | source lookup → source READ check → destination lookup → destination WRITE check. A denied source with a missing destination answers the policy's error. |
| XTEST CompareCursor [`Xext/xtest/xtest.c:120,133`] | window lookup → window check → cursor lookup → cursor check. A denied window with an invalid cursor answers the policy's error. |
| XIWarpPointer [`Xext/xinput/xiwarppointer.c:99,119`] | destination window lookup → destination GET_ATTR check → source window lookup → source GET_ATTR check (the device part is PR 5). A denied destination with an invalid source answers the policy's error. |
| XFIXES SelectSelectionInput [`Xext/xfixes/select.c`] | window lookup + GET_ATTR check before the event-mask BadValue. |
| XFIXES SelectCursorInput [`Xext/xfixes/cursor.c`] | window lookup + GET_ATTR check before the event-mask BadValue. |
| Composite Redirect/Unredirect Window/Subwindows [`Xext/composite/compext.c` `VERIFY_WINDOW`] | window lookup + check before the update BadValue and the root BadMatch. |
| RENDER CreatePicture [`Xext/render/picture.c` `CreatePicture` → `ChangePicture`] | the new picture's CREATE check (and drawable check, as today) before the alpha map and clip mask checks. |
| XInput XISetClientPointer [`Xext/xinput/xisetclientpointer.c:77`] | the window's resource checked with MANAGE (was GET_ATTR). XIGetClientPointer keeps GET_ATTR. Update the base plan's row (line ~209) and the spec's table if it names the mode. |

- [ ] **Step 1: Failing tests.** Assert in each policy row the exact policy error and value, the request's opcodes, no backend call and no state change; pair each with a no-policy row asserting today's outcome is unchanged.
  - ExpandRegion, CompareCursor, XIWarpPointer, SelectSelectionInput, SelectCursorInput: the earlier operand denied together with an invalid later field (missing destination region, invalid cursor, invalid source window, invalid event mask).
  - Composite: an invalid-update row (denied window + invalid update value) for each of the four minors (Redirect/Unredirect × Window/Subwindows); and a separate denied-root `RedirectWindow(root, Automatic)` row with a valid update, whose no-policy pair is today's BadMatch. Mutations, independently: the check after the update BadValue; the check after the root BadMatch.
  - CreatePicture: a missing alpha map is not a validity error (it translates to 0 and the picture is registered; keep a no-policy row asserting exactly that). Test the order on valid resources instead: through the shared recording path, the ASK row records CREATE (with the drawable's check as today) before the alpha map READ and before the clip mask READ; and a policy denying both the creation and the attribute answers the creation's denial. Cover both attributes. Mutation: check the attributes before the creation.
  - XISetClientPointer: the ASK row asserts MANAGE; XIGetClientPointer's stays GET_ATTR.
- [ ] **Step 2:** Run them; the new policy rows fail today (validity error or wrong check order/mode), the no-policy rows pass.
- [ ] **Step 3:** Move each check to right after its operand's own validation; change XISetClientPointer's mode.
- [ ] **Step 4:** All access and extension test modules pass. Mutation per site: move the check back after the later validation — its new row fails.
- [ ] **Step 5:** Update `docs/status.md`'s 3b entry only if it states an order or mode that changed.

### Task 3: A retained cursor's record lives until the backend destroys the cursor

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (new query), `backend/recording.rs`, `crates/yserver/src/kms/render/backend/trait_impl.rs` + `portable/cursor.rs`, `crates/yserver-core/src/resources/picture_font_cursor.rs` and `resources/cleanup.rs` (record lifetime), the core call sites of `cursor_host_released` / `free_cursor` / client cleanup.
- Test: `process_request/tests/access_cursors.rs`; a KMS backend test next to the existing cursor tests.

**Interfaces:**
- Produces: `fn cursor_alive(&self, host_xid: u32) -> bool` on the `Backend` trait. Default `false` (the backend destroys a cursor when `free_cursor` is called: the nested backend's behaviour today). KMS: true while the backend still holds the cursor (its `cursor_records` entry exists, i.e. `collect_released_cursors` has not destroyed it). RecordingBackend: true for hosts a test marks alive.

What must hold:
- The core drops a host cursor's record (`cursor_host_origins` entry) only when neither the core references it (`cursor_host_referenced`) nor the backend reports it alive. Today's drop points stay the trigger; a record kept because the backend still holds the cursor is dropped at a later drop point once the backend no longer holds it (each drop point sweeps records whose host is neither core-referenced nor alive). No other periodic work is added.
- With no policy installed nothing a client can observe changes (the record is read only by access checks).
- The nested backend's records behave exactly as today (default `false`).
- KMS host handles are never reused (`next_host_xid` is monotonic), so a kept record can never name a different cursor; state this in a comment where the record is kept.

- [ ] **Step 1: Failing tests (through `process_request`, RecordingBackend):**
  - `xfixes_get_cursor_image_checks_grab_cursor_after_free`: client A grabs the pointer with cursor C, frees C; the backend reports C alive and displayed; client B's `XFixesGetCursorImage` under a DENY policy answers the policy's error, and under ASK the target is C's XID with `target_owner` A. Mutation: drop the record at free time regardless of the backend.
  - `xfixes_get_cursor_image_checks_root_cursor_after_reset`: A sets C on the root window, frees C, resets the root's cursor; same assertions. Same mutation.
  - `cursor_record_dropped_once_backend_destroys_cursor`: after the backend reports C no longer alive, the next drop point (e.g. freeing another cursor) removes C's record, and a request reaching C's host finds no record. Mutation: never sweep kept records.
  - The same three for `GetCursorImageAndName`, as rows of one test if Task 1's form allows.
- [ ] **Step 2: Failing KMS backend test:** with the real KMS backend in the existing cursor test harness, a cursor set as the grab cursor stays `cursor_alive` after `free_cursor`, and stops after the grab cursor is cleared; same for an animated cursor's frame while the animated cursor is shown. Mutation: return `false` after `free_cursor`.
- [ ] **Step 3:** Implement the trait query, the KMS and RecordingBackend answers, and the record lifetime.
- [ ] **Step 4:** All tests pass; run the mutations above.
- [ ] **Step 5:** Update the base plan's retained-cursor note and the spec's retained-cursor note to say the record lives until the backend destroys the cursor.

## Verification (each task, then the whole branch)

The SDD gate in `~/Projects/claude-scratch/access-control/sdd-pr3b/gate.sh` (fmt, clippy ×3, FreeBSD, Windows check, workspace tests, xdmcp tests, each integration test file, ignored lavapipe suite, access/extension/RENDER modules 5× under `nproc` busy loops, `just man`), plus `tools/check-layout.sh`.
