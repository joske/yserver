# Access control layer, PR 3c-ii — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resource checks at every request of RANDR, Xinerama, RECORD and MIT-SCREEN-SAVER that names a resource,
with no change any client can observe while no policy is installed. This closes series row 3.

**Architecture:** New check sites only. Everything they call exists on master (`707c339f`): `ServerState::check_resource`,
`AccessContext::owner` for every kind used here (RANDR outputs, CRTCs, modes and providers answer `SERVER_OWNER`;
`RecordContext` its stored owner), `RequestOpcodes::for_request`, `validate_resource_access`,
`validate_resource_if_present`, `validate_arbitrary_resource_if_present` (the wide resolver of the 3c-i review round),
the shared `check_resource!` / `check_creation!` / `return_on_access_denial!` macros in `core_loop/process_request.rs`,
and every `AccessMode` bit used below.

**Tech Stack:** Rust, crate `yserver-core`; std only (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`, series row 3. Plans 3a, 3b, the 3b review
round, 3c-i (`2026-11-09-access-control-pr3c-i.md`) and its review round (`2026-11-10-access-control-pr3c-i-review1.md`)
are the earlier parts of the same row.

**Reference:** Xorg (MIT) at `/data/Projects/xorg-drivers/xserver` (`eae2c21`); file paths below are relative to it.
The source inventory cites XLibre lines; the access modes in these files are the same in Xorg (compared per file). Where the plan states an order, the reviewer checks it against Xorg.

**Source inventory:** `~/Projects/claude-scratch/access-control/pr3c/inventory.md` (taken at `f83615a4`; the RANDR,
RECORD, ScreenSaver, Xinerama and XRes sources are unchanged since, so its line numbers hold), §2 (RECORD, RANDR,
XRes, Xinerama, MIT-SCREEN-SAVER), §3, §4, §5, §6. The implementer locates sites by function name.

## Global Constraints

- With no policy installed, every byte a client observes stays the same, including error precedence, replies,
  events, backend calls and RANDR notifications. The existing suite passes unmodified.
- No new validity errors: an operand yserver does not validate today is checked only when its id resolves to an
  object of the expected kind (`validate_resource_if_present`), and an id that does not resolve keeps today's
  behaviour exactly. Zero (`None`) is never asked for a RANDR, RECORD or ScreenSaver operand, even where a resolver
  could match it (a RANDR output can report CRTC 0).
- Each check runs immediately after its operand's own existing validation, before any later validation of other
  fields. The order in which yserver validates operands is kept, even where Xorg's differs. An operand with no
  validation today is checked at the position Xorg looks it up relative to the request's other operands.
- Where one pure helper validates several operands (`RandrState::validate_set_crtc_config`,
  `validate_provider_output_source`, `validate_provider_offload_sink`, RECORD `check_client_specs`), each operand's
  check still runs right after that operand's own lookup inside the helper's order, not after the whole helper. The
  validation logic is not duplicated to achieve this; how the check reaches the helper (a callback, a split, an extra
  parameter) is the implementer's choice.
- Reuse first: every site calls the existing helpers and macros; no second resolver, no new `AccessMode` bit, no new
  `ResourceKind`, no RANDR lease, monitor or event-selection kind.
- No environment variable, no `cfg(test)` inside production function bodies. Test policies live in test modules and
  reuse the shared recording and decision path (`process_request/tests/extensions.rs`, `ExtensionHook` / `check`,
  `RecordingExtensionPolicy`).
- Client checks (all of XRes, RECORD's client-base specs) and device checks are PR 5; send and receive (RANDR and
  RECORD event and stream delivery) are PR 4.
- Licensing: Xorg and XLibre are MIT and may be read; no copyleft source is consulted. No code is copied.
- Tests exercise production paths through `process_request`. Every new test names the mutation that makes it fail,
  and every mutation is run and compiles. Suites run under CPU load. Test files stay under 6000 lines
  (`tools/check-layout.sh`).

## Scope decisions

The scope decisions of 3a, 3b and 3c-i apply unchanged: the policy's error goes out verbatim; `SERVER_OWNER` work is
not checked; server objects are checked like any other target (a real client's request on a RANDR output, CRTC, mode
or provider is asked, with owner `SERVER_OWNER`); modes are Xorg's at each site (the reference is Xorg; XLibre's modes are identical in every file this plan touches, checked at `eae2c21`); policies see an extension request's
minor opcode from the data byte. The denial test contract of plan 3a and the row form of the 3b review round apply to
every row. Rulings new to this plan:

- **RANDR property reads that delete** (`GetOutputProperty`, and `GetProviderProperty` up to its
  `BadImplementation`) ask the Output (Provider) `READ | WRITE` in one check when the raw `delete` byte is nonzero,
  and `READ` otherwise. Xorg asks `WRITE` alone (`randr/rrproperty.c:626`, `randr/rrproviderproperty.c:599`); the spec's rule "requests that read and delete ask for both modes"
  binds. The raw byte decides, before yserver's BOOL validation of it, as Xorg's `stuff->delete`. This is the one mode in this plan that differs from Xorg, and only under a policy.
- **RANDR requests yserver does not implement** keep today's answer and ask nothing when they validate no operand:
  `CreateMode` (`BadImplementation`), `CreateLease` (`BadImplementation`), `FreeLease` (`BadValue`, no lease store).
  The stubs that succeed today (`DestroyMode`, `AddOutputMode`, `DeleteOutputMode`) and the provider-property
  requests that validate their Provider before `BadImplementation` are checked: their operands resolve against live
  objects, and a denial answers the policy's error.
- **`SetMonitor`'s outputs are not checked:** Xorg copies them without a lookup. Monitors are not resources; only
  the root window is checked.
- **RANDR `SelectInput`** checks the window `RECEIVE` (Xorg `randr/rrdispatch.c:83`). Xorg's derived `RREvent` record has no yserver
  counterpart and is not checked.
- **`SetCrtcConfig`** asks, in Xorg's operand order mapped onto yserver's validation order: the CRTC `SET_ATTR`,
  if present, first (yserver never validates it on its own); each output `SET_ATTR` in wire order, right after that
  output resolves and before its "drives this CRTC" `BadMatch`; the mode `SET_ATTR` right after it resolves in the
  mode table. A disable (mode 0) asks the CRTC only. No backend call follows a denial.
- **RECORD client specifiers:** a specifier that is a client's resource base (or one of the special values) is a
  client, PR 5. Any other specifier that passes today's occupancy check is asked `GET_ATTR` through
  `validate_arbitrary_resource_if_present`, inside `check_client_specs`, in wire order, after the recorder `BadMatch`
  and occupancy `BadValue` for that specifier. An occupied id the wide resolver cannot name is not asked. A denial
  answers the policy's error before canonicalization, so registration and removal lists are unchanged.
- **RECORD contexts** are asked `USE` for every request naming an existing context (Xorg's `VERIFY_CONTEXT`),
  right after `BadContext` and before any other validation of the request. `CreateContext` asks `CREATE`,
  `Creation { parent: None }`, after the ID, spec and range validation and before the insert.
- **ScreenSaver `SetAttributes` value resources:** after today's owner-conflict `BadAccess` and before the attributes
  are stored, in ascending mask-bit order, each if present: background pixmap `READ` (not `None` or
  `ParentRelative`), border pixmap `READ` (not `CopyFromParent`), colormap `USE`, cursor `USE` (not `None`). These
  are Xorg's original-operand checks. A denial answers the policy's error and leaves the stored attributes as they
  were. Xorg's PanoramiX wrappers have no counterpart (yserver has no multi-screen wrapper), and the separate saver
  hooks are excluded by the spec.
- **ScreenSaver effective operands are checked at activation, for the installing client (sol r1 B-1, r2 B-2;
  option A, chosen by the user 2026-11-10).** Activation does not create the window from the original values: it adds
  override-redirect and replays Xorg's 64-bit `unsigned long` → `XID *` shift (`create_screen_saver_window`), so a
  scalar the client sent (a pixel) can reach `CreateWindow`, run as `SERVER_OWNER`, as a background pixmap, border
  pixmap, colormap or cursor; and yserver stores raw XIDs, so an id that resolved to nothing at `SetAttributes` can
  resolve to a resource created later. The stored attributes are deferred work of the client that installed them
  (`SaverAttributes::client`). So, when policies are installed, `create_screen_saver_window` asks, after it has
  computed the values it passes to `CreateWindow` and before that call, each effective background pixmap `READ`
  (not `None` / `ParentRelative`), border pixmap `READ` (not `CopyFromParent`), colormap `USE` and cursor `USE` (not
  `None`) that resolves, as `attrs.client`, with the request opcodes of `SetAttributes` (MIT-SCREEN-SAVER major,
  minor 3). These are derived targets: a denied one is replaced by its role's neutral value (`None` for a background
  pixmap and a cursor, `CopyFromParent` for a border pixmap and a colormap) and the window is created with the rest;
  no error goes anywhere (there is no request to answer). If the replacement makes `CreateWindow` fail, no saver
  window is shown, as today when `CreateWindow` fails. With no policy the activation path is byte-identical (no
  check, no replacement). Everything else in activation stays unchecked server work. The `SetAttributes` checks of
  the original operands (previous ruling) stay as they are; the r1 install-time check of effective operands and the
  shared materialization function are dropped, since this check covers both cases in one place.
- **XRes has no site in this PR:** QueryClients, QueryClientResources, QueryClientPixmapBytes and QueryClientIds
  check clients (PR 5); QueryResourceBytes has no resource hook in Xorg. Listed so a reviewer sees it was
  considered.

## Site table

"After X" means immediately after X's existing validation; "if present" means checked only when it resolves and is
not zero. Modes are Xorg's (`include/dixaccess.h`), joined with `|`.

### RANDR (major 128)

| Request | Checks, in order |
|---|---|
| 2 SetScreenConfig | drawable `WRITE`, after `BadDrawable` |
| 4 SelectInput | window `RECEIVE`, after `BadWindow` |
| 5 GetScreenInfo, 6 GetScreenSizeRange, 8 GetScreenResources, 25 GetScreenResourcesCurrent, 31 GetOutputPrimary, 32 GetProviders, 42 GetMonitors | window `GET_ATTR`, after `BadWindow` (GetScreenResources: before the backend reprobe) |
| 7 SetScreenSize | window `GET_ATTR`, after `BadWindow` (which follows today's parse `BadValue`) |
| 43 SetMonitor, 44 DeleteMonitor | window `GET_ATTR`, after `BadWindow`, before the name atom check |
| 9 GetOutputInfo, 10 ListOutputProperties, 11 QueryOutputProperty, 12 ConfigureOutputProperty, 13 ChangeOutputProperty, 14 DeleteOutputProperty | output `READ`, after `BadOutput` (13: after today's mode/format/length checks, which precede the output) |
| 15 GetOutputProperty | output `READ`, or `READ \| WRITE` on a nonzero raw delete byte, after `BadOutput`, before the property atom check; the malformed-body empty reply asks nothing |
| 17 DestroyMode | mode `DESTROY`, if present |
| 18 AddOutputMode, 19 DeleteOutputMode | output `READ`, if present; then mode `USE`, if present |
| 30 SetOutputPrimary | window `GET_ATTR`, after `BadWindow`; output `READ`, if not `None`, after `BadOutput` |
| 20 GetCrtcInfo, 22 GetCrtcGammaSize, 23 GetCrtcGamma, 24 SetCrtcGamma, 26 SetCrtcTransform, 27 GetCrtcTransform, 28 GetPanning, 29 SetPanning | CRTC `READ`, after `BadCrtc`, before any later check (24: before the size/data checks; 26: before the matrix and filter checks; 29: before the active-panning answer) |
| 21 SetCrtcConfig | per the `SetCrtcConfig` ruling |
| 33 GetProviderInfo | provider `READ`, after `BadProvider` |
| 34 SetProviderOffloadSink | provider `READ` after its lookup, before its capability check; sink `READ`, if not `None`, after its lookup, before its capability check |
| 35 SetProviderOutputSource | provider `READ` after its lookup, before its capability check; source `READ`, if not `None`, after its lookup, before its capability check; before the backend call |
| 36–40 List/Query/Configure/Change/DeleteProviderProperty | provider `READ`, after `BadProvider`, before `BadImplementation` |
| 41 GetProviderProperty | provider `READ`, or `READ \| WRITE` on a nonzero raw delete byte, after `BadProvider`, before `BadImplementation` |
| 16 CreateMode, 45 CreateLease, 46 FreeLease | nothing (unsupported, no operand validated) |

### Xinerama (major 151)

| Request | Checks |
|---|---|
| 1 GetState, 2 GetScreenCount, 3 GetScreenSize | window `GET_ATTR`, after `BadWindow` |

### RECORD (major 154)

| Request | Checks, in order |
|---|---|
| 1 CreateContext | resource specifiers `GET_ATTR` per the RECORD ruling; new context `CREATE`, `Creation { parent: None }`, before the insert |
| 2 RegisterClients | context `USE` after `BadContext`; resource specifiers per the ruling |
| 3 UnregisterClients | context `USE` after `BadContext`; resource specifiers per the ruling, before the deletion loop |
| 4 GetContext, 5 EnableContext, 6 DisableContext, 7 FreeContext | context `USE`, after `BadContext` (5: before the already-enabled `BadMatch`) |

### MIT-SCREEN-SAVER (major 150)

| Request | Checks, in order |
|---|---|
| 1 QueryInfo, 2 SelectInput | drawable `GET_ATTR`, if present (window first, then pixmap, as 3c-i's drawable ruling) |
| 3 SetAttributes | drawable `GET_ATTR`, after `BadDrawable`; then the original value resources per the first ScreenSaver ruling |
| (activation) | the effective value operands, as the installing client, per the second ScreenSaver ruling |
| 4 UnsetAttributes | drawable `GET_ATTR`, after `BadDrawable`, before the owner-specific unset |

## Review Focus

1. Mixed failures inside a multi-operand helper: a denied earlier operand with an invalid later one answers the
   policy's error (SetCrtcConfig denied output + unknown mode; SetProviderOutputSource denied provider + source
   without capability; RECORD denied first resource spec + recorder-named second spec).
2. No-policy parity for every operand yserver does not validate today (SetCrtcConfig's CRTC, the RANDR mode stubs,
   ScreenSaver QueryInfo/SelectInput drawables and SetAttributes value resources): an id that does not resolve, or
   resolves to another kind, or is zero, is not asked and answers exactly as today.
3. Effects on denial: SetCrtcConfig, SetOutputPrimary and SetProviderOutputSource make no backend call and emit no
   notification; ChangeOutputProperty / DeleteOutputProperty / GetOutputProperty with delete change no property and
   emit no `PropertyNotify`; ScreenSaver SetAttributes keeps the old attributes; RECORD Register/Unregister keep the
   context's lists; CreateContext inserts nothing.
4. Borrows: checks run before the mutable property stores in output.rs and before RECORD's mutable context.
5. Server work stays unchecked: RANDR emitters and deferred CRTC completion, RECORD stream delivery and
   FutureClients expansion, ScreenSaver activation apart from its effective value operands (the saver window's
   creation, mapping, cycle and destruction).

---

### Task 1: RANDR screen, window and Xinerama rows

**Files:** `core_loop/process_request/randr_ext/screen.rs`, `monitor.rs`, `provider.rs` (GetProviders only),
`output.rs` (GetOutputPrimary only), `xinerama.rs`; tests in a new `process_request/tests/access_randr.rs`,
registered in `tests/mod.rs`.

**Sites:** RANDR 2, 4, 5, 6, 7, 8, 25, 31, 32, 42, 43, 44; Xinerama 1–3.

- [ ] **Step 1: Failing tests:** DENY and ASK rows for every site (ASK asserts client, id, kind, mode and
  `RequestOpcodes`); `set_screen_config_pixmap_drawable_is_asked_as_pixmap`;
  `get_screen_resources_denied_does_not_reprobe` (no backend reprobe recorded; mutation: check after the reprobe);
  `set_monitor_denied_adds_no_monitor` (no monitor, no root ConfigureNotify); `set_screen_size_bad_value_precedes_check`
  (a bad size with a denied window answers today's `BadValue`; mutation: check before the parse).
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `feat(core): access control PR 3c-ii, Task 1: RANDR screen and Xinerama`.

### Task 2: RANDR outputs, output properties and modes

**Files:** `randr_ext/output.rs`, `randr_ext.rs` (mode stubs); tests in `access_randr.rs`.

**Sites:** RANDR 9–15, 17, 18, 19, 30.

- [ ] **Step 1: Failing tests:** DENY and ASK rows (ASK asserts `target_owner` = `SERVER_OWNER` for an output);
  `get_output_property_delete_asks_read_write` (raw delete byte 2, a value the BOOL check rejects later, asks
  `READ | WRITE`; delete 0 asks `READ`; mutation: decide from the validated BOOL);
  `delete_output_property_denied_keeps_property` (property kept, no `PropertyNotify`);
  `change_output_property_bad_value_precedes_check` (bad mode with a denied output answers today's `BadValue`);
  `output_mode_stubs_unresolved_ids_are_not_asked` (deny-all policy, unknown output and mode: Success as today);
  `set_output_primary_none_output_is_not_asked`.
- [ ] **Steps 2–5.** Commit `… Task 2: RANDR outputs, properties and modes`.

### Task 3: RANDR CRTCs and SetCrtcConfig

**Files:** `randr_ext/crtc.rs`, `crates/yserver-core/src/randr.rs` (`validate_set_crtc_config`); tests in
`access_randr.rs`.

**Sites:** RANDR 20–24, 26–29, 21.

- [ ] **Step 1: Failing tests:** DENY and ASK rows; for SetCrtcConfig the ASK row records CRTC, each output and the
  mode in that order, all `SET_ATTR`; `set_crtc_config_denied_output_before_bad_mode` (denied first output + mode not
  in the output's list: the policy's error; no-policy row keeps `BadMatch(mode)`; mutation: check after the whole
  helper); `set_crtc_config_denied_crtc_before_arity` (denied CRTC + outputs with mode 0: the policy's error;
  mutation: check the CRTC after the helper); `set_crtc_config_denied_makes_no_backend_call`;
  `set_crtc_config_disable_asks_crtc_only`; `set_crtc_gamma_denied_before_size_check`.
- [ ] **Steps 2–5.** Commit `… Task 3: RANDR CRTCs`.

### Task 4: RANDR providers

**Files:** `randr_ext/provider.rs`, `crates/yserver-core/src/randr.rs` (the two relationship validators); tests in
`access_randr.rs`.

**Sites:** RANDR 33–41.

- [ ] **Step 1: Failing tests:** DENY and ASK rows;
  `set_provider_output_source_denied_provider_before_capability` (denied provider without the sink capability: the
  policy's error; no-policy row keeps today's error; mutation: check after the helper);
  `set_provider_output_source_denied_source_makes_no_backend_call`; `offload_sink_none_sink_is_not_asked`;
  `provider_property_denied_before_bad_implementation` (each of 36–41; 41 with a nonzero delete byte asks
  `READ | WRITE`).
- [ ] **Steps 2–5.** Commit `… Task 4: RANDR providers`.

### Task 5: RECORD

**Files:** `core_loop/record.rs` (`check_client_specs`, `check_register`, the request handler); tests in a new
`process_request/tests/access_record.rs`.

**Sites:** every RECORD row.

- [ ] **Step 1: Failing tests:** DENY and ASK rows (context ASK asserts its stored owner);
  `record_resource_spec_extension_resource_is_asked` (a spec naming another client's SYNC counter is asked
  `GET_ATTR` as `SyncCounter`; mutation: use the core-only resolver); `record_client_base_spec_is_not_asked`;
  `register_denied_spec_keeps_lists` (the context's registered clients and ranges unchanged, as GetContext reports);
  `record_denied_first_spec_before_second_spec_error` (denied first spec + a second spec naming the recorder: the
  policy's error; mutation: check after the whole spec loop); `create_context_denied_creates_nothing` (the id stays
  free; a later CreateContext with it succeeds with no policy); `enable_context_denied_before_bad_match`.
- [ ] **Steps 2–5.** Commit `… Task 5: RECORD`.

### Task 6: MIT-SCREEN-SAVER

**Files:** `core_loop/process_request/saver_dpms.rs`; tests in a new `process_request/tests/access_saver.rs`.

**Sites:** every MIT-SCREEN-SAVER row.

- [ ] **Step 1: Failing tests:** DENY and ASK rows; `saver_set_attributes_value_resources_in_mask_order` (ASK records
  drawable, background pixmap, border pixmap, colormap, cursor in that order); one row per value resource with its
  sentinel (`None` / `ParentRelative` / `CopyFromParent`) asking nothing; `saver_set_attributes_denied_keeps_old_attrs`
  (the client's earlier attributes stay stored and are the ones activation uses; mutation: store before the
  checks); `saver_owner_conflict_precedes_value_checks` (another client's attributes
  stored + denied pixmap: today's `BadAccess`); `saver_query_info_unresolved_drawable_is_not_asked`;
  `saver_activation_denied_shifted_border_pixmap_is_dropped` (sol r1 B-1: mask `0x7`, values `[0, P, 0]` with `P`
  a root-depth pixmap of another client the policy denies `READ` to the installing client: `SetAttributes` succeeds,
  activation asks `P` as `Pixmap` `READ` for the installing client with `SetAttributes`' opcodes, and the saver
  window is created with the border from its parent instead of `P`; mutation: check only the original operands);
  `saver_activation_resource_created_after_install_is_asked` (sol r2 B-2: the same values with `P` unused at
  `SetAttributes`, then the other client creates `P`, then activation: `P` is asked and dropped on denial;
  mutation: check at `SetAttributes` only); `saver_activation_no_policy_is_unchanged` (no policy: the saver window
  has `P` as its border, exactly as today); `saver_activation_unresolved_effective_operand_is_not_asked`.
- [ ] **Steps 2–5.** Commit `… Task 6: MIT-SCREEN-SAVER`.

### Task 7: Spec rows, status and gate

- [ ] **Step 1:** spec, "What a policy can see": RANDR outputs and modes join CRTCs and providers as server objects
  asked with owner `SERVER_OWNER`; "Property": RANDR property reads that delete ask the output or provider
  `READ | WRITE`; "Who is checked": the ScreenSaver's stored attributes are deferred work of the client that
  installed them, and activation checks their effective value operands for that client as derived targets (a
  denied one is replaced by its neutral value; no error); "A denial is interpreted by its site": a row for it;
  series row 3 marked complete with XRes and RECORD client specifiers deferred to PR 5.
- [ ] **Step 2:** `docs/status.md`: one entry at the top for PR 3c-ii.
- [ ] **Step 3: Gate:** `cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`, also with
  `--features tcp-transport` and `--features xdmcp`; FreeBSD clippy; `cargo check -p yserver-core --target
  x86_64-pc-windows-gnu`; `cargo test --workspace` and with `--features xdmcp`; each `crates/*/tests` file;
  `--features xdmcp -- --ignored` (lavapipe); the access test modules 5 runs under `nproc` busy loops;
  `tools/check-layout.sh`; `just man`.
- [ ] **Step 4: Commit** `docs: access control PR 3c-ii spec rows and status`.
