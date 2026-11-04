# Access control layer, PR 1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The access control interface, the policy list that survives resets, the admission record, the client
lifecycle notifications and the server hook at the three #321 sites, with no change any client can observe while
no policy is installed.

**Architecture:** A new module `access.rs` defines `AccessPolicy` (default-allow methods), `Decision`, `DenyError`,
`AccessMode`, `Admission` and a read-only `AccessContext`. The ordered policy list lives in `HostAccessHandle`
beside `auth`, so the existing reset carry-over moves it. Checks return `Allow` without a dynamic call when the list
is empty. `AuthState` gains `admit`, which returns how a client was admitted; the record rides
`Message::ClientSetupComplete` into `ClientState.admission`.

**Tech Stack:** Rust, crate `yserver-core`; std only (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md` (settled with the maintainer on #320,
2026-11-03). PR 1 is the first row of its series table.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence. The existing
  suite passes unmodified, including #321's tests (`process_request/tests/core_misc.rs:1074` onwards) and the reset
  and `run_core` host-access tests (`reset.rs:1110`, `run/tests.rs:4534`).
- No environment variable, no `YSERVER_*` variable, no `cfg(test)` inside production function bodies. Test-only
  policies live in test modules.
- Reuse first: new code only where the goal is impossible otherwise. The list rides the existing
  `HostAccessHandle` carry-over (no second carry-over path); lifecycle notifications sit next to RECORD's existing
  `client_started` / `client_disconnected` calls; owner-gone sits in the one function every zombie teardown goes
  through.
- `Allow` means only "this policy does not object". No policy can override another's denial; there is no grant
  answer. A check asks policies in order and stops at the first `Deny`.
- Checks never block: every `AccessPolicy` method is synchronous and returns a value, never a future or a pending
  state.
- The admission record never exposes the secret: no `Debug`, `Display` or accessor of `Admission`/`CredentialId`
  yields cookie bytes.
- Licensing: Xorg and XLibre are MIT and may be read; no copyleft source is consulted. No code is copied.
- Tests exercise production paths: requests go through `process_request`, admission through the setup thread,
  lifecycle through `handle_client_setup_complete`, `process_disconnect`, `KillClient` and `reset_generation`.
  Every new test names the mutation that makes it fail; threaded suites (setup thread, `run_core`) run 5 times under
  `nproc` busy loops with zero failures.

## Scope decisions (rulings for this plan)

- The trait gets only the methods PR 1 calls: `server`, the four lifecycle notifications. PRs 2 to 5 add their hook
  methods with default `Allow`, which breaks no implementor. Same for `AccessMode`: PR 1 defines `READ` and
  `MANAGE`; later PRs add bits.
- The context skeleton exposes only what PR 1's checks can use: the client's peer, its admission and the request's
  opcodes. Ownership, root identity and atom names arrive with the resource and property checks (PR 3).
- Policies are `Arc<dyn AccessPolicy>` with `AccessPolicy: Send + Sync`, so `ServerState` keeps its auto traits. A
  policy that keeps state uses its own interior mutability; methods take `&self`, which lets a check lend `&ServerState`
  to the context while the list is borrowed from the same state.
- The server hook runs before the locality rule at the #321 sites (spec, "Server"), as Xorg's `AuthorizedClient`
  (`os/access.c:1302`, 21.1.24) and XLibre do (maintainer, #328). The invalid-mode `BadValue` stays first. With no
  policy, `may_manage_access` answers `BadAccess` for a non-local client exactly as before; with a policy, a remote
  attempt reaches it.
- A reset's teardown already runs every client through `process_disconnect` and every zombie through
  `destroy_zombie_resources_reporting` (`reset.rs:215-248`), so policies hear `connection_gone` and `owner_gone` for
  each of them, then `generation_ended` at the successful return of `reset_generation`, after its last refusal
  point (the default font reinstall, `reset.rs:468-470`). A reset that refuses (the server shuts down) sends no
  `generation_ended`.
- `client_connected` is sent right after `handle_client_setup_complete` inserts the `ClientState`
  (`run.rs:4638`), before the fallible reader spawn (`run.rs:4686-4695`). A failed spawn goes through the normal
  disconnect (`run.rs:2532-2554`), so a policy always hears `client_connected` before `connection_gone`. RECORD's
  `client_started` and the reset trigger's establishment arming stay where they are: the reset design arms only
  once setup reached Running (`2026-09-09-server-reset-design.md:297-305`), which is a different boundary.
- `CredentialId` is allocated, not derived (maintainer, #320): `AuthState` holds a counter that only goes up,
  and each credential gets the next id when it is loaded, stored beside its bytes (file cookies and the session
  credential alike). A reload that still holds a cookie keeps that cookie's id; a cookie that leaves the file, a
  replaced or cleared session credential and the cookies a reset drops (#324) take their ids with them, and an id
  is never handed out again. The id says nothing about the cookie's bytes.

## Review Focus

1. A policy installed before `run_core` vanishing at startup because `run.rs:2127` rebuilds the handle → Task 1 test.
2. A policy that denies `ListHosts` while the handler holds `clients.get_mut` (borrow order at `misc.rs:329`) → the
   check moves before the mutable borrow; Task 4 test.
3. A RetainPermanent client: `connection_gone` at disconnect but `owner_gone` only when its zombie is destroyed, and
   never twice (KillClient by resource, KillClient AllTemporary, rootless host and reset all destroy zombies) → Task 3.
4. A client admitted by the host list after its cookie failed must record `HostAccess`, not the failed cookie → Task 2.
5. The internal clipboard-bridge client never goes through `AuthState`; it must still get an admission record and
   lifecycle notifications, and keep its authorization exemption and its exclusion from reset arming and XDMCP
   (`run.rs:2555-2557`) unchanged → Tasks 2 and 3.

---

### Task 1: Interface, policy list, reset carry-over

**Files:**
- Create: `crates/yserver-core/src/access.rs` (and `pub mod access;` in `lib.rs`)
- Modify: `crates/yserver-core/src/core_loop/auth.rs:94-110` (`HostAccessHandle`)
- Modify: `crates/yserver-core/src/core_loop/run.rs:2127` (keep installed policies)
- Modify: `crates/yserver-core/src/core_loop/reset.rs:447` (carry the list)
- Modify: `crates/yserver-core/src/server.rs` (installer)
- Test: `crates/yserver-core/src/core_loop/reset.rs` tests, `crates/yserver-core/src/core_loop/run/tests.rs`

**Interfaces:**
- Produces, in `access.rs`:
  - `pub enum Decision { Allow, Deny(DenyError) }`
  - `pub struct DenyError { pub code: u8, pub value: u32 }` with `pub const ACCESS: DenyError` (`BAD_ACCESS`, 0)
  - `pub struct AccessMode(u32)` with `pub const READ`, `pub const MANAGE`, `contains`, `|`
  - `pub struct RequestOpcodes { pub major: u8, pub minor: u8 }`
  - `pub struct AccessContext<'a>` (fields private) with `pub fn peer(&self, client: ClientId) -> Option<&PeerIdentity>`
    and `pub fn request(&self) -> Option<RequestOpcodes>`. `admission` is added by Task 2, with the field it reads;
    Task 1 has no placeholder.
  - `pub trait AccessPolicy: Send + Sync` with, all defaulting to allow / no-op:
    `fn server(&self, ctx: &AccessContext<'_>, client: ClientId, mode: AccessMode) -> Decision`,
    `fn client_connected(&self, ctx: &AccessContext<'_>, client: ClientId, peer: &PeerIdentity, admission: &Admission)`,
    `fn connection_gone(&self, ctx: &AccessContext<'_>, client: ClientId)`,
    `fn owner_gone(&self, client: ClientId)`, `fn generation_ended(&self)`
- Produces, on `HostAccessHandle`: `pub policies: Vec<Arc<dyn AccessPolicy>>`; on `ServerState`:
  `pub fn install_access_policy(&mut self, policy: Arc<dyn AccessPolicy>)` (appends);
  `pub(crate) fn check_server(&self, client: ClientId, mode: AccessMode, request: Option<RequestOpcodes>) -> Decision`
  (empty list → `Allow` without building a context; else asks in order, stops at first `Deny`);
  `pub(crate) fn notify_policies(&self, f: impl Fn(&dyn AccessPolicy, &AccessContext<'_>))` (every policy, in order).
- `Admission` is declared here as in the spec (`Cookie { credential: CredentialId, source: CredentialSource }`,
  `HostAccess`, `Internal`; `CredentialSource { File, Session }`; `CredentialId` opaque, `Copy + Eq + Hash + Debug`
  printing only the number); Task 2 produces its values.

- [ ] **Step 1: Write the failing tests**
  - `reset_generation_carries_installed_policies_in_order` (reset.rs tests, beside `:1110`): install policies A then
    B, reset, assert `state.host_access.policies` holds the same two `Arc`s in order (`Arc::ptr_eq`). Mutation: drop
    the `policies` line from the carry-over → fails.
  - `run_core_keeps_policies_installed_before_it` (run/tests.rs, beside `:4534`): install a policy, start as that
    test does, assert it is present after the handle is installed. Mutation: build the handle with an empty list →
    fails.
- [ ] **Step 2: Run them, expect FAIL** (compile error: no `install_access_policy`).
  Run: `cargo test -p yserver-core --lib policies_ -- --nocapture`
- [ ] **Step 3: Implement** the module, the field (empty by `Default`), the installer, `check_server`,
  `notify_policies`, the `run.rs:2127` change (keep `std::mem::take(&mut state.host_access.policies)`) and the
  `reset.rs:447` carry (clone the `Vec` of `Arc`s).
- [ ] **Step 4: Run, expect PASS**; also the two existing host-access carry tests unchanged.
- [ ] **Step 5: Commit** `feat(core): access control interface and policy list`

### Task 2: Admission record

**Files:**
- Modify: `crates/yserver-core/src/core_loop/auth.rs` (`admit`, credential ids)
- Modify: `crates/yserver-core/src/core_loop/setup_thread.rs:176` (call `admit`, send the record)
- Modify: `crates/yserver-core/src/core_loop/message.rs:211` (`admission` field on `ClientSetupComplete`)
- Modify: `crates/yserver-core/src/core_loop/run.rs:2519`, `:4618` (carry into `ClientState`)
- Modify: `crates/yserver-core/src/server.rs:3098` (`pub admission: Admission` on `ClientState`) and every
  `ClientState { .. }` literal (test helpers get `Admission::Internal`)
- Test: `crates/yserver-core/src/core_loop/setup_thread.rs` tests, `auth.rs` tests

**Interfaces:**
- Consumes: `Admission`, `CredentialId`, `CredentialSource` (Task 1).
- Produces: `AccessContext::admission(&self, client: ClientId) -> Option<&Admission>`, reading `ClientState.admission`.
- Produces: `pub fn admit(&self, peer: &PeerIdentity, setup_generation: Generation, proto_name: &[u8], proto_data: &[u8]) -> Result<Admission, &'static str>`
  on `AuthState`. `check` keeps its signature and becomes `admit(..)` mapped to `AuthVerdict`; its callers and tests
  do not change.
- Mapping, decided at the exact branch that admits: session credential → `Cookie { source: Session }`; file cookie
  match → `Cookie { source: File }`; local-open or access-disabled fallback (`Inner::verdict`) and the
  selfhost/SI list (`list_admits_unix`) → `HostAccess`; the internal client (`setup_thread.rs:175`, `internal`) →
  `Internal` without calling `admit`.

- [ ] **Step 1: Write the failing tests**, through the setup thread as its existing tests do (`setup_thread.rs:502`,
  `:586`), reading `admission` from the `ClientSetupComplete` message:
  - `two_file_cookies_admit_with_distinct_stable_credentials`: an `-auth` file with cookies C1, C2; connect with C1
    twice and C2 once; C1's two ids are equal, C2's differs, both `source: File`. Mutation: a constant id → fails.
  - `session_cookie_admits_as_session`: install a session cookie, connect with it → `source: Session`. Mutation:
    report `File` → fails.
  - `failed_cookie_then_host_list_admits_as_host_access`: Unix peer, wrong cookie, local access open → `HostAccess`.
    Mutation: record the presented cookie → fails.
  - `internal_client_admits_as_internal`: through `Message::InternalClient` → `Internal`.
  - `credential_ids_are_kept_while_loaded_and_never_reused` (auth.rs, through `admit` and the file reload): a reload
    that keeps C1 keeps its id; C1 removed from the file, then put back → a new id; a reset (#324 drops the cookies)
    and the reload after it → a new id for the same bytes; a session credential replaced by another → a new id; no id
    seen in the test repeats. Mutation: derive the id from the cookie bytes → fails.
  - `access_context_returns_the_clients_admission`: a connected client's admission is what the context reports.
    Mutation: return `None` → fails.
  - `admission_debug_never_shows_the_cookie` (auth.rs): `format!("{:?}", admission)` for a known cookie does not
    contain its bytes in hex or decimal.
- [ ] **Step 2: Run, expect FAIL.** Run: `cargo test -p yserver-core --lib admit`
- [ ] **Step 3: Implement** `admit`, the id counter and the id stored beside each credential, the message field, the
  `ClientState` field and `AccessContext::admission`.
- [ ] **Step 4: Run, expect PASS**, plus the whole `auth`, `xdmcp` and `setup_thread` test modules unchanged, 5 runs
  under load.
- [ ] **Step 5: Commit** `feat(core): record how each client was admitted`

### Task 3: Lifecycle notifications

**Files:**
- Modify: `crates/yserver-core/src/core_loop/run.rs` `handle_client_setup_complete` (connected, right after the
  `state.clients.insert` at `:4638`, before the reader spawn)
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs` (connection gone next to
  `record::client_disconnected`; owner gone at the end of a non-retaining disconnect, and in
  `destroy_zombie_resources_reporting` on its `Ok` path)
- Modify: `crates/yserver-core/src/core_loop/reset.rs` (generation ended at the successful return of
  `reset_generation`, after the default-font reinstall)
- Test: `crates/yserver-core/src/core_loop/process_disconnect.rs` tests, `reset.rs` tests, `run/tests.rs`

**Interfaces:**
- Consumes: `notify_policies`, the trait methods (Task 1), `ClientState.admission` (Task 2).
- Invariants: per client, `client_connected` → `connection_gone` → `owner_gone`, each exactly once; `owner_gone`
  for a retaining client only when `destroy_zombie_resources_reporting` returns `Ok`, which is Xorg's
  `ClientStateGone` for a retained client (spec, "Client state", item 3). Freeing the zombie's resources one by one
  (`FreePixmap` from another client) sends nothing, as in Xorg; `generation_ended` after every
  per-client notification of the reset. Every policy hears every notification, including one that denied something.

- [ ] **Step 1: Write the failing tests** with a recording policy (test module) that appends `(event, client)`:
  - `lifecycle_of_a_destroying_client`: connect through `handle_client_setup_complete`, disconnect → `[connected(peer,
    admission), connection_gone, owner_gone]`. Mutation: drop the connected call → fails.
  - `lifecycle_of_a_retained_owner`: client sets RetainPermanent (`SetCloseDownMode` through `process_request`),
    creates a pixmap, disconnects → `[connected, connection_gone]`; another client `KillClient`s the pixmap →
    `owner_gone` once. Mutation: emit `owner_gone` at disconnect regardless of retain → fails.
  - `retain_temporary_owner_gone_through_all_temporary`: as above with RetainTemporary and `KillClient(AllTemporary)`.
  - `freeing_a_zombie_resource_does_not_end_its_owner`: RetainPermanent client with one pixmap disconnects; another
    client `FreePixmap`s it → no `owner_gone`; then `reset_generation` → exactly one `owner_gone` for it. Mutation:
    emit `owner_gone` from the resource free path → fails.
  - `reset_tells_policies_after_every_client`: two clients, one retained zombie; `reset_generation` → each client's
    `connection_gone`/`owner_gone`, then exactly one `generation_ended`, last; the policy object is the same `Arc`
    after the reset. Mutation: emit `generation_ended` before teardown → fails.
  - `failed_reader_spawn_still_connects_before_departing`: force the reader spawn in
    `handle_client_setup_complete` to fail (as the existing establishment-failure tests do, or by the cheapest
    production-faithful means the code offers; say which in the report), let the caller's disconnect run →
    `[connected, connection_gone, owner_gone]`. Mutation: move the connected call after the spawn → fails.
  - `refused_reset_sends_no_generation_end`: make the default-font reinstall fail in `reset_generation` → no
    `generation_ended`. Mutation: emit it when the fresh state is installed → fails.
  - `every_policy_hears_lifecycle`: two recording policies, the first denying every check, both record the same
    sequence.
  - A leak/end-state check closes each scenario: no `ClientState`, no zombie entry left for the departed client.
- [ ] **Step 2: Run, expect FAIL.** Run: `cargo test -p yserver-core --lib lifecycle`
- [ ] **Step 3: Implement** the four call sites.
- [ ] **Step 4: Run, expect PASS**, plus `process_disconnect`, `reset` and rootless host tests unchanged.
- [ ] **Step 5: Commit** `feat(core): tell access policies about client lifecycle`

### Task 4: Server hook at the #321 sites

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/misc.rs:300-460` (`handle_list_hosts`,
  `handle_change_hosts`, `handle_set_access_control`)
- Test: `crates/yserver-core/src/core_loop/process_request/tests/core_misc.rs` (beside `:1074`)

**Interfaces:**
- Consumes: `check_server`, `AccessMode::{READ, MANAGE}`, `RequestOpcodes`, `DenyError` (Task 1).
- Order per request, all existing steps unchanged: `ChangeHosts` and `SetAccessControl`: mode `BadValue` →
  `check_server(MANAGE)` → existing `may_manage_access` `BadAccess` → the rest. `ListHosts`: existing `may_read_access` →
  `check_server(READ)` → reply. The `ListHosts` check runs before `state.clients.get_mut`. A `Deny(e)` emits
  `e.code` with `e.value` and the request's major opcode (109, 110, 111), minor 0.

- [ ] **Step 1: Write the failing tests**, table-driven through `process_request`:
  - `server_hook_denies_each_host_access_request`: rows `ListHosts`, `ChangeHosts` insert, `SetAccessControl`;
    a policy denying with `DenyError { code: BAD_VALUE, value: 0x55 }` (not `BadAccess`, to prove the policy's error
    is reported); assert the error bytes and that the host list and access mode are unchanged
    (`list_hosts` afterwards with no policy). Mutation: ignore the decision → fails.
  - `server_hook_asks_with_client_mode_and_context`: a recording policy sees client 1, `MANAGE` for 109 and 111,
    `READ` for 110, `request() == Some(RequestOpcodes { major, minor: 0 })`, and the client's peer and admission
    through the context. Mutation: pass `READ` for `ChangeHosts` → fails.
  - `invalid_mode_answers_before_the_hook_and_the_hook_before_locality`: for `ChangeHosts` and `SetAccessControl`,
    mode 2 → `BadValue` and the recording policy was not asked; a non-local client with an allowing policy →
    `BadAccess` after the policy was asked once; a non-local client with a denying policy → the policy's error.
  - `several_policies_on_one_request`: allow-then-deny and deny-then-allow on `ListHosts` are both denied with the
    first denier's error, and a policy after a denier is not asked.
  - `revocation_applies_from_the_next_check`: a policy holding an `AtomicBool`, allow then flip, two `ListHosts` →
    reply then error.
  - `empty_list_bytes_unchanged`: the existing #321 tests pass unmodified (no new assertion; listed for the gate).
- [ ] **Step 2: Run, expect FAIL.** Run: `cargo test -p yserver-core --lib server_hook`
- [ ] **Step 3: Implement** the three checks.
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** `feat(core): ask access policies at the host access requests`

### Task 5: Gate

- [ ] `cargo +nightly fmt` first, then `cargo +nightly fmt -- --check` (AGENTS.md).
- [ ] `cargo clippy --all-targets -- -D warnings`, exactly as CI (AGENTS.md), and the FreeBSD cross-check:
  `source ~/Projects/claude-scratch/freebsd-sysroot/env.sh && cargo clippy -p yserver-core --target x86_64-unknown-freebsd`.
- [ ] `cargo test --workspace`, `cargo test --workspace --features xdmcp` (the ordinary XDMCP tests, which the
  `--ignored` run skips), each `crates/*/tests/*.rs`, and `cargo test --workspace --features xdmcp -- --ignored`
  (lavapipe suite, as CI).
- [ ] The setup-thread, `run_core` and lifecycle tests 5 times under `nproc` busy loops, zero failures.
- [ ] `just man`.
- [ ] One regression run on AMD and on NVIDIA by the coordinator, with the user's approval (spec, Testing):
  start, open clients, `xhost` list/add/remove, close; nothing changes.
