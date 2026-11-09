# Detectable auto-repeat and XI2 key repeat — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A held key's software auto-repeat reaches each client the way Xorg delivers it: a client that enabled XKB
DetectableAutoRepeat gets repeated KeyPress events with no fake KeyRelease, and an XI2 client gets KeyPress with
`XIKeyRepeat` and no release. Fixes #355 (holding Tab in Marvel Rivals under Proton behaves as a turbo key).

**Architecture:** The auto-repeat stops emitting a KeyRelease + KeyPress pair and emits one KeyPress marked as a
repeat. Delivery decides per client: core and XI1 forms get a fake KeyRelease in front of the repeated press unless
the client enabled DetectableAutoRepeat; the XI2 form carries `XIKeyRepeat` and never a fake release. `PerClientFlags`
moves from the backend's stateless reply to the core, which stores the flags per client.

**Tech Stack:** Rust, crates `yserver-core` and `yserver` (KMS backend input path); std only.

**Spec:** this plan; the design was approved by the user in chat on 2026-11-09 after measuring the defect.

**Evidence:** `~/Projects/claude-scratch/issue-355/run4-nv-manual/` (real Tab held on a test server, master
68fa4be2 + VRR branch, NVIDIA): without the flag 64 KeyPress / 64 KeyRelease; with `XkbSetDetectableAutoRepeat`
answered "supported, set" 21 KeyPress / 21 KeyRelease — identical pattern, a KeyRelease+KeyPress pair every ~40 ms.
Probe: `~/Projects/claude-scratch/issue-355/repeat-probe.c`.

**Reference behaviour (Xorg, MIT, `/data/Projects/xorg-drivers/xserver`; read, never copy):**
- `xkb/xkbAccessX.c` `AccessXKeyboardEvent`: a repeat is ONE key press event with `key_repeat` set.
- `dix/events.c` ~2098–2128 (`TryClientEvents` path): for a core `KeyPress` / XI1 `DeviceKeyPress` that is a key
  repeat, a fake `KeyRelease` / `DeviceKeyRelease` copy is written to the client first, unless the client has
  `XkbPCF_DetectableAutoRepeatMask` in its per-client XKB flags (`_XkbWantsDetectableAutoRepeat`).
- `dix/eventconvert.c` ~716: the XI2 `XI_KeyPress` of a repeat has `XIKeyRepeat` set; no XI2 release is generated.
- `dix/eventconvert.c` ~148 / ~367: the core / XI1 conversions mark the repeat (`EventSetKeyRepeatFlag`).
- `xkb/xkb.c` `ProcXkbPerClientFlags`: stores the client's flags (`change` / `value` masks) and replies with the
  supported mask and the client's resulting flags (plus auto-reset controls, out of scope here).

## Global Constraints

- A client that never enabled DetectableAutoRepeat and selects core or XI1 key events receives exactly the bytes it
  receives today for a held key (fake release, then press, same fields, same order relative to other clients'
  deliveries).
- Device input (libinput, XTEST) delivery is unchanged; only software auto-repeat changes. XI2 raw key events stay as
  today (auto-repeat produces none: `key_fanout.rs` `raw_key_event_to_state` doc).
- Grabs, focus, frozen-device queueing and replay keep working for repeats exactly as for today's pair; a repeat that
  is queued behind a frozen keyboard keeps its repeat marking when replayed.
- `PerClientFlags`: the reply's supported mask stays `0x1f`; the stored flags are per client, survive across requests,
  and are dropped with the client. Only DetectableAutoRepeat changes behaviour; the other four flags are stored and
  reported, nothing more.
- The nested host-X11 backend (`host_x11`) keeps its current behaviour; it is out of scope.
- No `YSERVER_*` variable, no option to restore the old behaviour, no `cfg(test)` in production bodies.
- Reuse first: extend the existing repeat path (`fire_pending_repeats_at`, `HostInputEvent::KeyRepeat`,
  `handle_host_key(.., repeat, ..)`) and the existing delivery functions (`deliver_key_to_window`,
  `deliver_key_to_grab_owner`, the XI1 slave delivery, `encode_key_xi2_for_device`); new state only where needed, each
  piece justified in the report.
- Tests go through production paths: `process_request` for `PerClientFlags`, and `fire_pending_repeats_at` with a real
  held key for delivery (the existing repeat tests are the pattern). Every test names the mutation that makes it fail;
  every mutation is run. Suites under CPU load. Test files stay under 6000 lines.
- Licensing: Xorg is MIT and may be read; no code copied.

## Review Focus

1. Mixed clients on one window: A with DetectableAutoRepeat, B without, C selecting XI2 — each gets its own form of
   the same repeat, in one delivery.
2. A grab owner (core active grab, XI2 grab) receives the repeat in its own form; a passive grab activation does not
   turn a repeat into a new press-grab trigger differently from today.
3. Repeat stop: the real release when the key goes up is delivered to every client exactly once, flagged or not.
4. A client that enables the flag, then disables it (`change` with `value` 0), gets the fake releases again.
5. `PerClientFlags` from a client of the other byte order replies correctly.

---

### Task 1: One marked repeat press; per-client fake release for core and XI1; PerClientFlags state

**Files:** `crates/yserver-core/src/core_loop/run.rs` (`fire_pending_repeats_at`), the key event type carried by
`HostInputEvent::KeyRepeat` and the KMS backend entry (`crates/yserver/src/kms/render/backend/portable/devices.rs`
`handle_host_key`), `crates/yserver-core/src/core_loop/key_fanout.rs` (core, grab-owner and XI1 delivery),
`crates/yserver-core/src/server.rs` (`ClientState`: per-client XKB flags), `core_loop/process_request/xkb.rs`
(`PerClientFlags`, minor 21, answered in the core), and the backend reply it replaces
(`crates/yserver/src/kms/xkb.rs` `reply_per_client_flags`, `portable/keyboard.rs` minor 21). Tests next to the
existing auto-repeat and key-fanout tests.

**Interfaces:**
- Produces: the repeat marking on the key event that reaches delivery (Task 2 reads it for XI2); the per-client flag
  accessor used by delivery.

- [ ] **Step 1: Failing tests**
  - `per_client_flags_are_stored_and_reported` (process_request): enable DetectableAutoRepeat, then a second request
    with `change` 0 reports it still set; a third with `change`=1, `value`=0 clears it; a different client's reply is
    unaffected. Mutation: answer `value & change` of the request only (today's behaviour).
  - `held_key_without_flag_gets_release_press_pairs`: a client selecting core KeyPress|KeyRelease on the focus window
    gets, per repeat, a KeyRelease then a KeyPress with today's fields. Mutation: drop the fake release for everyone.
  - `held_key_with_detectable_flag_gets_presses_only`: same with DetectableAutoRepeat enabled through
    `process_request`: only KeyPress per repeat, and exactly one KeyRelease when the key goes up. Mutation: ignore the
    stored flag.
  - `mixed_clients_get_their_own_repeat_form`: two clients on one window (one flagged) in the same repeat.
  - `xi1_device_key_repeat_honours_flag`: an XI1 `DeviceKeyPress` selector gets the fake `DeviceKeyRelease` only
    without the flag.
  - `repeat_to_grab_owner_honours_flag`: a core keyboard grab owner with the flag gets presses only.
- [ ] **Step 2:** run; the flag tests fail, the no-flag tests pass today and must keep passing.
- [ ] **Step 3:** implement: the repeat emits one marked press; delivery writes the fake release per client.
- [ ] **Step 4:** run the new tests, every existing auto-repeat / key-fanout / XKB test, and each mutation.
- [ ] **Step 5: Commit** `fix(xkb): honour DetectableAutoRepeat per client (#355)`.

### Task 2: XI2 key repeat flag

**Files:** `core_loop/key_fanout.rs` (`encode_key_xi2_for_device`, the XI2 forms in delivery); tests beside Task 1's.

**Interfaces:** Consumes Task 1's repeat marking.

- [ ] **Step 1: Failing tests**
  - `xi2_key_repeat_has_flag_and_no_release`: an `XI_KeyPress|XI_KeyRelease` selector on the focus window gets, per
    repeat, one `XI_KeyPress` with `XIKeyRepeat` (`flags` bit 16) and no `XI_KeyRelease`; the first press and the real
    release have no `XIKeyRepeat`. Mutation: leave `flags` 0; emit a fake XI2 release.
  - `xi2_shadows_core_for_repeat`: a client selecting both XI2 and core key events still gets only the XI2 form of a
    repeat (today's shadowing rule).
- [ ] **Steps 2–4** as Task 1. **Step 5: Commit** `fix(xi2): mark repeated key presses with XIKeyRepeat (#355)`.

### Task 3: Status and gate

- [ ] **Step 1:** `docs/status.md`: one entry at the top for #355 (cause measured, Xorg behaviour, what changes for
  flagged and XI2 clients, unchanged for the rest).
- [ ] **Step 2: Gate:** `cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked
  --features xdmcp`; the whole ignored suite `--workspace --locked --features xdmcp -- --ignored` (lavapipe env);
  `cargo clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp -- -D
  warnings`; FreeBSD clippy; `tools/check-layout.sh`; `just man`; the new and existing key/XKB test modules 5 runs
  under 16 busy loops.
- [ ] **Step 3: Commit** `docs(status): detectable auto-repeat and XI2 key repeat (#355)`.

**Hardware check (coordinator, with the user's OK):** `repeat-probe` (core with and without the flag, XI2) against
the branch on a test server: flagged run = N presses and 1 release; XI2 run = presses with `XIKeyRepeat`, 1 release.
