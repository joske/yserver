# Access control layer, PR 2 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The extension access check (`QueryExtension`, `ListExtensions`) and the extension dispatch check (one site
in `process_request`), with no change any client can observe while no policy is installed.

**Architecture:** `AccessPolicy` gains two default-allow methods. `ServerState` gains two check helpers that share
the existing first-denial loop with `check_server`. Discovery asks the access check per extension; dispatch asks the
dispatch check once per request whose major opcode belongs to an available extension. Both sites interpret a denial
as Xorg does: the extension looks absent.

**Tech Stack:** Rust, crate `yserver-core`; std only (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`. PR 2 is the second row of its series
table ("Extension access and dispatch checks"). PR 1 (#328, merged as 4ccb1d2a) provides `access.rs`,
`ServerState::install_access_policy`, `check_server` and `notify_policies`.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence. The existing
  suite passes unmodified, in particular `xcmisc_advertised_in_query_and_list_extensions`
  (`process_request/tests/core_misc.rs:2463`), the transport gate's tests (`process_request/tests/dri3_shm.rs`,
  `tcp_tests.rs`) and PR 1's access tests.
- Check helpers return `Allow` without a dynamic call, without building an `AccessContext` and without looking the
  extension up when the policy list is empty.
- No environment variable, no `YSERVER_*` variable, no `cfg(test)` inside production function bodies. Test-only
  policies live in test modules.
- Reuse first: the new helpers reuse `check_server`'s first-denial loop; extension availability is decided by one
  function shared by `QueryExtension`, `ListExtensions` and dispatch (today the same `match` is written twice in
  `process_request/misc.rs:239-278`).
- `Allow` means only "this policy does not object". A check asks policies in order and stops at the first `Deny`.
  Checks never block.
- Licensing: Xorg and XLibre are MIT and may be read; no copyleft source is consulted. No code is copied.
- Tests exercise production paths: every request goes through `process_request`. Every new test names the
  mutation that makes it fail.

## Scope decisions (rulings for this plan)

- **Hook signatures.** Both hooks receive the extension's registered name, as XLibre's hooks receive the
  `ExtensionEntry`. The dispatch hook's minor opcode comes from `ctx.request()`, as the spec says ("the opcodes are
  the question"). Neither hook takes an `AccessMode`; the spec's hook table gives them none.
- **Which requests the dispatch check covers.** Only requests whose major opcode is the `major_opcode` of an entry
  in `nested::EXTENSIONS` that is available on this backend. Core requests (1-127), unassigned majors and
  extensions the backend does not offer are not asked, as in Xorg, where `GetExtensionEntry` returns nothing for
  them and the hook does not run (`Xext/xace.c:41-44`; XLibre `dix/dispatch.c:553-560`).
- **A dispatch denial is `BadRequest`**, whatever `DenyError` the policy gave: Xorg maps every hook failure to
  `BadRequest` ("pretend extension doesn't exist", `Xext/xace.c:45-46`). XLibre reports the policy's status
  instead; we follow Xorg (client-observable = Xorg parity). The error carries value 0, the request's minor opcode
  (`header.data`) and its major opcode, the shape `emit_x11_error_with_minor` already gives the gate's errors. This
  matches the spec's "extension access → reported as absent": a client denied an extension sees the same thing at
  discovery and at dispatch.
- **Where the dispatch check sits.** After the transport gate (spec, "The existing transport gate") and after the
  maximum-length `BadLength`, immediately before the core-length and exact-length validation. In Xorg the
  maximum-length check precedes the hook (`dix/dispatch.c:546-549`) and the per-request size checks run inside the
  handler, after it. yserver's core-length, exact-length and value-mask validators only apply to core opcodes
  (`request_lengths.rs:990-993`), so for an extension request this position is equivalent to "right after the
  gate" except for the over-maximum `BadLength`, which keeps Xorg's precedence. The spec's sentence "the dispatch
  check runs right after it" gets this clarification in Task 2.
- **The gate keeps precedence over a denial.** A non-local client's DRI3 request answers the gate's `BadMatch` even
  with a denying policy, and the policy is not asked. In Xorg the locality test sits in the DRI3 handler, after the
  hook, so a denying hook there would answer `BadRequest`; the spec deliberately keeps the gate first and
  unchanged, and the difference is observable only with a denying policy, which no production build installs
  before PR 6.
- **Discovery denial.** `QueryExtension` on a denied extension answers exactly the reply of an unknown name
  (`present = 0`, opcode, first event and first error 0), as Xorg's `ProcQueryExtension` does when
  `ExtensionAvailable` fails (`dix/extension.c:214-246`). `ListExtensions` leaves denied names out and keeps the
  order of the others (`dix/extension.c:270-300`). The access check runs per entry, after the backend availability
  test, so an extension the backend does not offer is never asked about.
- **Discovery and dispatch are independent.** Denying access does not deny dispatch, and the reverse; SECURITY
  (PR 6) installs both.
- **Out of scope:** PR 1's four follow-ups (`owner_gone` order, `owner_gone` on fatal teardown, `DenyError::code`
  validation, `ClientState.internal` vs `Admission::Internal`) stay separate. The hot-path benchmark of the spec's
  Testing section belongs to the resource and receive checks (PRs 3-4), whose sites are hot; the dispatch check's
  empty-list cost is one `is_empty` branch.

## Review Focus

1. A request replayed through `process_request` later (held during a server grab, `reset.rs:1333`,
   `record.rs:1080`, `process_disconnect.rs:1939`, `host_x11/mod.rs`) is checked when it is actually dispatched,
   with the opcodes it carries; nothing caches an earlier answer. Covered by the revocation test in Task 2.
2. `ListExtensions` with every entry denied answers a well-formed empty list (`nExtensions = 0`, length 0), not an
   error. Task 1 test.
3. An extension whose availability depends on the backend (RENDER, XKEYBOARD, DRI3, GLX under
   `RecordingBackend`) is never asked about at discovery or dispatch when unavailable, and its existing answer is
   unchanged. Tasks 1 and 2 tests.
4. A denied BIG-REQUESTS `Enable` must not switch the reader to big framing: the client state and the reader
   control channel are untouched. Task 2 test.
5. Name matching is exact and case-sensitive, as Xorg's (`strcmp`): a policy denying `"XFIXES"` does not affect
   `"xfixes"` (unknown anyway) nor any other entry. Task 1 test.

---

### Task 1: Extension access check at discovery

**Files:**
- Modify: `crates/yserver-core/src/access.rs` (trait methods)
- Modify: `crates/yserver-core/src/server.rs:1643-1680` (helpers; `check_server` reuses the shared loop)
- Modify: `crates/yserver-core/src/core_loop/process_request/misc.rs:239-278, 732-783`
- Create: `crates/yserver-core/src/core_loop/process_request/tests/extensions.rs`
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/mod.rs` (`mod extensions;`)

**Interfaces:**
- Consumes (PR 1): `AccessContext::new(&ServerState, Option<RequestOpcodes>)`, `ctx.request()`, `ctx.peer()`,
  `ctx.admission()`, `Decision`, `DenyError`, `RequestOpcodes { major, minor }`,
  `ServerState::install_access_policy`.
- Produces:
  - `AccessPolicy::extension_access(&self, ctx: &AccessContext<'_>, client: ClientId, extension: &str) -> Decision`
    (default `Decision::Allow`).
  - `AccessPolicy::extension_dispatch(&self, ctx: &AccessContext<'_>, client: ClientId, extension: &str) -> Decision`
    (default `Decision::Allow`; called from Task 2).
  - `ServerState::check_extension_access(&self, client: ClientId, extension: &str, request: Option<RequestOpcodes>) -> Decision`
  - `ServerState::check_extension_dispatch(&self, client: ClientId, extension: &str, request: Option<RequestOpcodes>) -> Decision`
  - `ServerState::policies_installed(&self) -> bool` (`pub(crate)`; `!self.host_access.policies.is_empty()`).
  - `misc::extension_available(ext: &ExtensionMetadata, backend: &mut dyn Backend) -> bool` (`pub(super)`), the
    single availability rule now used by `extension_query_reply` and `advertised_extension_names`.

- [ ] **Step 1: Write the failing tests** in `tests/extensions.rs`, using `RecordingBackend`, `install_client` and
  the read helpers the other test files use. Policies: a recording policy (records client, extension name,
  `ctx.request()`, peer, admission; answers a configurable `Decision`), a name-set denier, and a revocable policy
  (an `AtomicBool`).
  - `query_extension_denied_answers_like_an_unknown_name` — policy denies `"XFIXES"` with
    `DenyError { code: BAD_VALUE, value: 0x55 }`. The `QueryExtension("XFIXES")` reply is byte-identical to the
    reply for `"NO-SUCH-EXTENSION"` with no policy (`present = 0`, three zero bytes), and no error is written.
    Mutation: drop the check → `present = 1` fails it.
  - `list_extensions_leaves_out_denied_names` — policy denies `"XFIXES"` and `"SHAPE"`. The reply lists exactly
    the no-policy list without those two, in the same order, with `nExtensions` and the reply length matching.
    A second case denies every name: `nExtensions = 0`, reply length 0. Mutation: ignore the decision.
  - `extension_access_asks_once_per_available_extension` — recording policy that allows. `QueryExtension("SYNC")`
    records one call `{client 1, "SYNC", Some(RequestOpcodes { major: 98, minor: 0 }), peer, admission}`;
    `ListExtensions` records one call per advertised name, in advertised order, each with `{ major: 99, minor: 0 }`;
    `QueryExtension("RENDER")` (unavailable under `RecordingBackend`) and `QueryExtension("xfixes")` record nothing.
    Mutation: ask before the availability test, or pass the wrong opcode.
  - `extension_access_allow_keeps_bytes` — with an allow-all policy, `QueryExtension` for every advertised name
    and `ListExtensions` give the same bytes as with no policy. Mutation: build the reply from the wrong branch.
  - `extension_access_first_denial_wins` — on `QueryExtension("XFIXES")`: allow-then-deny and deny-then-allow
    both answer absent; with deny first, the second policy records no call. Mutation: keep asking after a denial.
  - `extension_access_revocation_applies_to_the_next_request` — revocable policy: allowed `QueryExtension("XFIXES")`
    answers present, then after flipping it answers absent, on the same connection.
  - `extension_access_denial_does_not_deny_dispatch` — access denies `"XFIXES"`; an XFIXES `QueryVersion`
    (major `XFIXES_MAJOR_OPCODE`, minor 0) still gets its normal reply.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p yserver-core --lib core_loop::process_request::tests::extensions`
Expected: compile failure (no `extension_access` method), then, with the trait methods stubbed, the denial tests
fail with `present = 1` and the full list.

- [ ] **Step 3: Implement**
  - Trait methods in `access.rs`, default `Decision::Allow`, docs saying what each is asked about.
  - In `server.rs`, a private `first_denial(&self, request: Option<RequestOpcodes>, ask: impl Fn(&dyn AccessPolicy,
    &AccessContext<'_>) -> Decision) -> Decision` holding the existing empty-list shortcut and loop;
    `check_server`, `check_extension_access` and `check_extension_dispatch` call it. `policies_installed`.
  - In `misc.rs`, `extension_available` replaces the two `match` blocks; `handle_query_extension` treats a denied
    available extension as absent; `handle_list_extensions` filters denied names. The `RequestOpcodes` are
    `{ major: 98, minor: 0 }` and `{ major: 99, minor: 0 }`. With an empty list neither handler calls the helper
    per entry (use `policies_installed`).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p yserver-core --lib core_loop::process_request::tests` then `cargo test --workspace`
Expected: all pass, the existing suite unmodified.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/access.rs crates/yserver-core/src/server.rs \
  crates/yserver-core/src/core_loop/process_request/misc.rs \
  crates/yserver-core/src/core_loop/process_request/tests/extensions.rs \
  crates/yserver-core/src/core_loop/process_request/tests/mod.rs
git commit -S -m "feat(core): access control PR 2: extension access check at discovery"
```

### Task 2: Extension dispatch check

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:252-330` (one site, after the max-length check)
- Modify: `crates/yserver-core/src/core_loop/process_request/misc.rs` (lookup by major opcode)
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/extensions.rs`
- Modify: `docs/superpowers/specs/2026-10-30-access-control-layer-design.md` (site table and "The existing transport
  gate": add the dispatch row and the position after the maximum-length check)

**Interfaces:**
- Consumes (Task 1): `check_extension_dispatch`, `policies_installed`, `extension_available`,
  `AccessPolicy::extension_dispatch`.
- Produces: `misc::available_extension_by_opcode(major: u8, backend: &mut dyn Backend) -> Option<&'static ExtensionMetadata>`
  (`pub(super)`).

- [ ] **Step 1: Write the failing tests** in `tests/extensions.rs`.
  - `every_extension_dispatch_denial_answers_bad_request` — table over every `EXTENSIONS` entry available under
    `RecordingBackend`: a fresh state, a policy whose `extension_dispatch` denies with
    `DenyError { code: BAD_VALUE, value: 0x55 }`, one request with the entry's major opcode, minor 0 and an 8-byte
    body. The client receives exactly one 32-byte error: code `BAD_REQUEST`, the sequence, value 0, minor 0, the
    major opcode; no reply; `backend.calls()` is empty. Mutation: report the policy's error, or skip the check.
  - `denied_big_requests_enable_changes_nothing` — denied BIG-REQUESTS `Enable`: `BadRequest`,
    `big_requests_enabled` stays false, and the client's reader-control channel receives nothing.
  - `extension_dispatch_asks_with_client_name_and_opcodes` — recording policy that allows. An XFIXES `QueryVersion`
    records `{client 1, "XFIXES", Some(RequestOpcodes { major: XFIXES_MAJOR_OPCODE, minor: 0 })}` and gets its
    normal reply. `GetInputFocus` (43), an unassigned major (200) and RENDER's major (unavailable under
    `RecordingBackend`) record nothing, and their bytes equal the no-policy bytes. Mutation: ask for core opcodes,
    or for unavailable extensions.
  - `transport_gate_and_max_length_answer_before_the_dispatch_check` — denying recording policy. A non-local
    client's DRI3 request answers the gate's `BadMatch` (same bytes as without a policy); a request whose
    `length_units` exceeds the maximum answers `BadLength`; neither records a call. Mutation: move the check above
    the gate or above the maximum-length check.
  - `extension_dispatch_first_denial_wins_and_revocation_applies` — deny-then-allow: `BadRequest`, second policy
    not asked; allow-then-deny: `BadRequest`. A revocable policy allows an XFIXES `QueryVersion`, then after
    flipping denies the next one on the same connection.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p yserver-core --lib core_loop::process_request::tests::extensions`
Expected: the dispatch tests fail with normal replies instead of `BadRequest`.

- [ ] **Step 3: Implement**
  - `available_extension_by_opcode` in `misc.rs`: the entry whose `major_opcode` matches and that
    `extension_available` accepts.
  - In `process_request`, after the maximum-length `BadLength` and before the core-length validation: only when
    `header.opcode >= 128` and `state.policies_installed()`, look the extension up; if found, ask
    `check_extension_dispatch(client_id, ext.name, Some(RequestOpcodes { major: header.opcode, minor: header.data }))`
    and on `Deny(_)` return `emit_x11_error_with_minor(state, client_id, sequence, BAD_REQUEST, 0,
    u16::from(header.data), header.opcode)`.
  - Spec: in "A denial is interpreted by its site", add the row "Extension dispatch | `BadRequest` with value 0, as
    Xorg (`Xext/xace.c:45-46`), whatever the policy's error"; in "The existing transport gate", say the dispatch
    check runs after the gate and after the maximum-length `BadLength`, as in Xorg (`dix/dispatch.c:546-549`).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p yserver-core --lib core_loop::process_request::tests` then `cargo test --workspace`
Expected: all pass, the existing suite unmodified.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs \
  crates/yserver-core/src/core_loop/process_request/misc.rs \
  crates/yserver-core/src/core_loop/process_request/tests/extensions.rs \
  docs/superpowers/specs/2026-10-30-access-control-layer-design.md
git commit -S -m "feat(core): access control PR 2: extension dispatch check"
```

### Task 3: Gate

- [ ] **Step 1:** Run the PR 1 gate script adapted to this worktree (`gate.sh`: `cargo +nightly fmt -- --check`;
  clippy plain, `--features tcp-transport`, `--features xdmcp`, FreeBSD; `cargo test --workspace` and
  `--features xdmcp`; each `crates/*/tests` target; `--features xdmcp -- --ignored` on lavapipe; `just man`).
  The loaded runs are `cargo test -p yserver-core --lib core_loop::process_request::tests::extensions` 5 times
  under `nproc` busy loops.
  Expected: every step rc=0, zero failures in the loaded runs.
- [ ] **Step 2:** Hardware regression pass on AMD and NVIDIA with the PR 1 harness (`hw-ac.sh amd|nvidia`), plus
  `xdpyinfo` extension list compared with the PR 1 run's list.
  Expected: identical extension list, 12/12 hostprobe checks, clean exit on both GPUs.
