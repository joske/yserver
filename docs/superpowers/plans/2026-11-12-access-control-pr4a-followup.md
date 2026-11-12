# Access control layer, PR 4a follow-up — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address the maintainer's review of PR #363 before 4b: the two `SendEvent` resolvers stop walking at the first
delivery, the GenericEvent description is tested without relying on an XI validation gap, unassigned extension codes are
documented, the Xorg citations are pinned to xorg-server 21.1.24, and the `SetCrtcConfig` validator comment states its
real order. The 4b–4e denial rulings (selection, `MappingNotify`, SYNC alarms, Present) are already written into the
spec by the commit that adds this plan.

**Architecture:** The core and XI resolvers become lazy walks (iterators) so the request loop, which already stops at
the first delivery, also stops the walk. No behaviour a client can observe changes, with or without a policy.

**Tech Stack:** Rust, crate `yserver-core`; std only.

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md` (series row 4; "Who is checked"; "Where the
checks go" → Receive / Send).

**Reference:** Xorg (MIT) xorg-server 21.1.24 at `/data/Projects/xorg-21.1.24`; paths below are relative to it. Where
the spec decides, it binds; where it is silent, Xorg is followed.

**Review being addressed:** joske on PR #363 (merged as `ab880d30`), items 1–6 listed in the Tasks below.

## Global Constraints

- With no policy installed, every byte a client observes stays the same. With a policy installed, every hook call
  (order, arguments, count) stays the same as at `ab880d30`. The existing suite passes unmodified except for the one
  test Task 2 rewrites.
- No new validity errors. The listed `SendEvent` / `SendExtensionEvent` validation gaps stay as they are (they are a
  separate fix).
- Reuse first: no new resolver, no new type besides what a step names. No environment variable, no `cfg(test)` in
  production bodies.
- Tests exercise production functions. Every new test names the mutation that makes it fail, and every mutation is run
  and compiles. Suites run under CPU load. Test files stay under 6000 lines.
- Licensing: Xorg is MIT and may be read; no code copied.

## Review Focus

1. A resolver error (`BadWindow`, `BadValue` for propagate) is still raised before any send check, exactly as before:
   errors are decided eagerly, only the walk is lazy.
2. The rootless callers (`rootless/host.rs` `send_client_message_as_server`, `rootless/selection.rs`
   `send_selection_notify`) use mask 0 and keep their single-candidate result.
3. A policy that denies every recipient at a window still makes the walk continue to the parent (the 4a rule); the
   lazy walk must not end early in that case.
4. The XI walk's `0..256` bound and its do-not-propagate class filtering keep their order relative to the candidate it
   yields.
5. A citation fix never changes a claim, only the line numbers or file; a claim that does not hold in 21.1.24 is
   reported, not silently reworded.

---

### Task 1: Lazy `SendEvent` resolvers (review item 1)

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/selection.rs` (`send_event_recipients`, `handle_send_event`)
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/mod.rs` (`xi1_send_extension_event_resolve_targets`)
- Modify: `crates/yserver-core/src/core_loop/process_request/xi/dispatch/events.rs` (its caller)
- Modify: `crates/yserver-core/src/rootless/host.rs`, `crates/yserver-core/src/rootless/selection.rs` (callers)

**Interfaces:**
- Produces: `send_event_recipients(state: &ServerState, destination, event_mask: u32, propagate: u8) ->
  Result<impl Iterator<Item = SendEventCandidate> + '_, (u8, u32)>` — destination resolution and the propagate check
  stay eager and return `Err` before any candidate exists; each `next()` computes one candidate.
- Produces: `xi1_send_extension_event_resolve_targets(...) -> impl Iterator<Item = SendEventCandidate> + '_`, same
  arguments.
- `SendEventCandidate` is unchanged.

- [ ] **Step 1:** Convert both resolvers to lazy walks (e.g. `std::iter::from_fn` over the existing loop state). Each
  candidate's `recipients` and `continue_propagation` are computed exactly as now; the walk yields nothing after a
  candidate whose `continue_propagation` is false.
- [ ] **Step 2:** Adapt the two request callers: the loop takes candidates from the iterator and ends at the first
  candidate with an allowed recipient (as now); fanout, which needs `&mut ServerState`, runs after the loop with the
  recipients the loop chose. Adapt the two rootless callers (mask 0, one candidate).
- [ ] **Step 3:** There is no new test: the change is internal work only and no output differs. The existing
  `access_send.rs` rows (denial at the first, a middle and the last candidate; all selectors denied; zero-mask creator)
  and the `core_misc.rs` / XI propagation tests pin the behaviour. Run them; all pass.
- [ ] **Step 4:** Commit `perf(access): SendEvent resolvers walk lazily and stop at the first delivery`.

### Task 2: GenericEvent and unassigned-code descriptions (review items 2 and 3)

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request/tests/access_send.rs`
- Modify: `crates/yserver-core/src/access.rs` (doc comments of `EventDescription` and `EventDescription::core`)
- Modify: `crates/yserver-core/src/core_loop/process_request/selection.rs` (doc comment of `describe_wire_event`)

- [ ] **Step 1:** Rewrite `xi_generic_event_metadata_uses_sender_byte_order_and_denial` so its `SendExtensionEvent`
  carries only events Xorg accepts (`EXTENSION_EVENT_BASE <= type < lastEvent`, `Xi/sendexev.c:150-158`); keep its
  byte-order and denial assertions for the XI event. Rename it if the name no longer describes it.
- [ ] **Step 2:** Add tests that call `describe_wire_event` directly with the fixture's backend:
  - `describe_generic_event_reads_evtype_in_sender_byte_order`: a type-35 event with byte 1 = `0xf2` and bytes 8–9 =
    `0x1234` in big-endian order describes as `EventDescription::extension(35, 0xf2, 0x1234)`; the same bytes read in
    little-endian order describe as `extension(35, 0xf2, 0x3412)`; a type byte `35 | 0x80` describes the same.
    Mutation: read bytes 8–9 in a fixed byte order, or describe type 35 as core.
  - `describe_unassigned_extension_code_has_no_extension_metadata`: an event code ≥ 64 inside no extension's live
    range describes with `type_code` equal to that code and `extension: None`; an XI event code in range describes
    with XI's major opcode. Mutation: map the unassigned code into the nearest extension.
- [ ] **Step 3:** Document on `EventDescription` and `describe_wire_event`: a core event has `type_code < 64` and
  `extension: None`; `extension: None` with `type_code >= 64` is a code no extension has assigned (core `SendEvent`
  accepts such codes today); `type_code == 35` with `extension: Some` is a GenericEvent whose `major_opcode` is byte 1
  and `event_type` its `evtype`.
- [ ] **Step 4:** Run the new and rewritten tests and their mutations; commit
  `test(access): describe GenericEvent and unassigned codes directly`.

### Task 3: Citations pinned to 21.1.24; `SetCrtcConfig` comment (review items 4 and 6)

**Files:**
- Modify: `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`
- Modify: Xorg citations in the comments of `crates/yserver-core/src/core_loop/process_request/selection.rs`
  (`handle_convert_selection`, `handle_send_event`, `send_event_recipients`), `.../xi/mod.rs` and
  `.../xi/dispatch/events.rs` (SendExtensionEvent), `crates/yserver-core/src/access.rs`, and the doc comments of
  `.../tests/access_send.rs` and the SendEvent tests of `.../tests/core_misc.rs`
- Modify: `crates/yserver-core/src/randr.rs` (doc comment of `validate_set_crtc_config`)

- [ ] **Step 1:** Check every Xorg `file:line` citation in the files above against `/data/Projects/xorg-21.1.24` and
  correct the file or line range where it does not point at the code the sentence describes. Known: `ProcSendEvent`'s
  type range check is `dix/events.c:5568-5574`, its propagate check `:5617-5620`, its send checks and delivery
  `:5622-5641`; `ProcConvertSelection` writes its events at `dix/selection.c:290-311`; XI's `SendEvent` is
  `Xi/exevents.c:2905-2963`, with its only send check at `:2961`. Where a sentence's claim does not hold in 21.1.24 (not just its line), list it in the commit message
  and the report instead of rewording it.
- [ ] **Step 2:** Rewrite the `validate_set_crtc_config` doc comment: it checks arity and resolves outputs before the
  mode, while Xorg verifies the mode right after the CRTC (`randr/rrcrtc.c:1294-1320`); the error a client sees when
  both a mode and an output are bad therefore differs from Xorg (as documented in the PR 3c-ii plan).
- [ ] **Step 3:** `cargo +nightly fmt -- --check`; commit `docs(access): Xorg citations pinned to 21.1.24; SetCrtcConfig
  order stated`.

### Gate (after Task 3)

`cargo +nightly fmt -- --check`; `cargo clippy --all-targets -- -D warnings`, also with `--features tcp-transport` and
`--features xdmcp`; FreeBSD clippy (`~/Projects/claude-scratch/freebsd-sysroot/env.sh`); `cargo check -p yserver-core
--target x86_64-pc-windows-gnu`; `cargo test --workspace` and with `--features xdmcp`; each `crates/*/tests` file; the
lavapipe `--ignored` suite; the access test modules 5 times under CPU load; `tools/check-layout.sh`; `just man`.
