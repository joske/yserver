# Detectable auto-repeat and XI2 key repeat — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** While a key is held, a client that enabled XKB DetectableAutoRepeat stops receiving the auto-repeat's fake
KeyRelease, and an XI2 client gets repeated presses marked `XIKeyRepeat` with no fake release. Every other client
receives exactly what it receives today. Fixes #355 (holding Tab in Marvel Rivals under Proton behaves as a turbo
key) and the same symptom in Metal Gear Solid V.

**Architecture (option A, chosen by the user 2026-11-09 after sol plan review r1):** the auto-repeat keeps emitting
today's KeyRelease + KeyPress pair through the same input path, so admission, held state, XKB processing, grabs and
the frozen-device queue are untouched. Both events of the pair carry a "synthetic repeat" marker to delivery.
Delivery then:
- omits the synthetic release from each core / XI1 recipient that enabled DetectableAutoRepeat;
- omits the XI2 form of the synthetic release for every XI2 recipient, and sets `XIKeyRepeat` on the XI2 form of the
  synthetic press.
`PerClientFlags` moves from the backend's stateless reply to the core, which stores the flags per client.

**Not chosen (option B, Xorg's exact model):** one marked press per repeat with the fake release inserted at
serialization. Sol r1 showed it changes today's behaviour for unflagged clients (KeyPress-only and KeyRelease-only
selectors, passive-grab deactivation by the fake release) and needs new repeat admission past the KMS duplicate-press
guard, XKB cooking rules and a repeat marker in `Xi1QueuedEvent`. Recorded in the backlog for the Xorg parity work.

**Tech Stack:** Rust, crates `yserver-core` and `yserver` (KMS input path); std only.

**Spec:** this plan; design approved by the user in chat on 2026-11-09 after measuring the defect.

**Evidence:** `~/Projects/claude-scratch/issue-355/run4-nv-manual/` (real Tab held on a test server, NVIDIA): without
the flag 64 KeyPress / 64 KeyRelease; with `XkbSetDetectableAutoRepeat` answered "supported, set" 21 / 21 — the
same KeyRelease+KeyPress pair every ~40 ms. Probe: `~/Projects/claude-scratch/issue-355/repeat-probe.c`.

**Reference behaviour (Xorg, MIT, `/data/Projects/xorg-drivers/xserver`; read, never copy):**
- `dix/events.c` ~2098–2128: a flagged client (`_XkbWantsDetectableAutoRepeat`) gets no fake core `KeyRelease` / XI1
  `DeviceKeyRelease` for a repeat.
- `dix/eventconvert.c` ~716: the XI2 `XI_KeyPress` of a repeat has `XIKeyRepeat` (flags bit 16); XI2 gets no fake
  release.
- `xkb/xkb.c` `ProcXkbPerClientFlags`: stores the client's flags under the `change` mask and replies with the
  supported mask and the client's resulting flags.

## Global Constraints

- A client without DetectableAutoRepeat that selects core or XI1 key events receives exactly today's bytes and order
  for a held key, including KeyPress-only and KeyRelease-only selectors.
- The pair still goes through admission, held state, XKB processing, grab activation/deactivation and the frozen
  queue exactly as today; only the per-recipient write changes. A flagged client's skipped release still has every
  server-side effect it has today (accepted divergence from Xorg, documented in `docs/status.md`).
- The marker survives every deferred path the pair can take: a grab owner's delivery, the XI1 queued event
  (`Xi1QueuedEvent` through `pointer_fanout`) and the frozen-device replay. Device input (libinput, XTEST) never
  carries it. XI2 raw events stay as today.
- `PerClientFlags`: reply supported mask stays `0x1f`; flags are stored per client under `change`, persist across
  requests, are reported as stored, and go away with the client. Only DetectableAutoRepeat changes behaviour.
- `PerClientFlags` validates before any mutation, in Xorg's order (`xkb/xkb.c` `ProcXkbPerClientFlags`, and the
  `CHK_MASK_LEGAL` / `CHK_MASK_MATCH` macros): exact request size (`BadLength`); the client's XKB initialization
  (`UseExtension`) guard, as yserver already applies it to XKB requests; the keyboard `deviceSpec` (the existing XKB
  device-spec check, `BadKeyboard`); `change` within `0x1f` (`BadValue`, value `_XkbErrCode2(0x01, illegal bits)`);
  `value` within `change` (`BadMatch`, `_XkbErrCode2(0x02, value & !change)`); and, only when AutoResetControls is
  being set, `ctrlsToChange` within the boolean controls (`BadValue`, code 0x03), `autoCtrls` within `ctrlsToChange`
  (`BadMatch`, 0x04) and `autoCtrlValues` within `autoCtrls` (`BadMatch`, 0x05). A rejected request changes no stored
  flag. These errors are new client-visible behaviour (today the backend answers any request with a reply); they
  follow Xorg.
- Nested host-X11 backend (`host_x11`): out of scope, unchanged.
- No `YSERVER_*` variable, no option to restore the old behaviour, no `cfg(test)` in production bodies.
- Reuse first: the repeat path (`fire_pending_repeats_at`, `HostInputEvent::KeyRepeat`, `handle_host_key(.., repeat,
  ..)`), the existing delivery functions (`deliver_key_to_window`, `deliver_key_to_grab_owner`, the XI1 slave delivery
  and `pointer_fanout`'s XI1 writer, `encode_key_xi2_for_device`); new state only where needed, each piece justified.
- Tests are wire tests on the real KMS input path: a held key through the KMS backend's `handle_host_key` (not
  `RecordingBackend`, which only records calls), clients registered through `process_request`, bytes read from the
  clients' output. Every test names the mutation that makes it fail; every mutation is run. Suites under CPU load.
  Test files stay under 6000 lines.
- Licensing: Xorg is MIT and may be read; no code copied.

## Review Focus

1. Mixed recipients of one repeat on one window: flagged core, unflagged core, XI2 — each gets its own form.
2. Grab owner (core active grab, XI2 grab) with and without the flag; frozen keyboard (synchronous grab) then thaw:
   the replayed pair keeps the marker and the flagged owner still gets no release.
3. The real release when the key goes up reaches every client exactly once, flagged or not; it never carries the
   marker.
4. Enable then disable the flag (`change` 1, `value` 0): fake releases come back.
5. `PerClientFlags` from a client of the other byte order.

---

### Task 1: PerClientFlags state and the synthetic-release filter for core and XI1

**Files:** `core_loop/run.rs` (`fire_pending_repeats_at`), the key event carried by `HostInputEvent::KeyRepeat` and
the KMS entry (`crates/yserver/src/kms/render/backend/portable/devices.rs` `handle_host_key`),
`core_loop/key_fanout.rs` (core and grab-owner delivery, XI1 slave delivery), `core_loop/pointer_fanout.rs` and
`server.rs` (`Xi1QueuedEvent`, the XI1 writer, frozen replay), `server.rs` (`ClientState`: per-client XKB flags),
`core_loop/process_request/xkb.rs` (`PerClientFlags`, minor 21, answered in the core), replacing the backend reply
(`crates/yserver/src/kms/xkb.rs` `reply_per_client_flags`, `portable/keyboard.rs` minor 21). Tests next to the
existing auto-repeat / key-fanout / XI1 tests.

**Interfaces:** Produces the synthetic-repeat marker on the key event and its queued forms (Task 2 reads it), and a
per-client accessor for the stored XKB flags.

- [ ] **Step 1: Failing tests**
  - `per_client_flags_are_stored_and_reported` (process_request): enable, a second request with `change` 0 still
    reports it set, `change`=1 `value`=0 clears it, another client is unaffected, other byte order. Mutation: answer
    `value & change` of the request only.
  - `per_client_flags_rejects_before_mutation` (process_request): `change`=1 `value`=3 → `BadMatch` with Xorg's
    error value, then a query reports the flag still clear; also `change` outside `0x1f` (`BadValue`), a wrong size
    (`BadLength`) and a non-keyboard `deviceSpec`. Mutation: store before validating.
  - `held_key_unflagged_core_gets_todays_pairs`: wire bytes per repeat equal today's (release then press, same fields),
    also for a KeyPress-only and a KeyRelease-only selector. Mutation: drop the synthetic release for everyone.
  - `held_key_flagged_core_gets_presses_only`: per repeat only KeyPress; one KeyRelease when the key goes up.
    Mutation: ignore the stored flag.
  - `mixed_core_clients_get_their_own_form`: flagged and unflagged on one window in one repeat.
  - `xi1_repeat_honours_flag`: XI1 `DeviceKeyPress|Release` selectors, flagged and unflagged.
  - `flagged_grab_owner_gets_no_synthetic_release` and `frozen_keyboard_replay_keeps_marker` (synchronous grab, repeat
    while frozen, thaw: flagged owner gets presses only). Mutation: drop the marker from the queued form.
- [ ] **Step 2:** run; flagged rows fail, unflagged rows pass today and must keep passing.
- [ ] **Step 3:** implement.
- [ ] **Step 4:** new tests, every existing auto-repeat / key / XI1 / XKB / grab test, each mutation.
- [ ] **Step 5: Commit** `fix(xkb): honour DetectableAutoRepeat per client (#355)`.

### Task 2: XI2 key repeat

**Files:** `core_loop/key_fanout.rs` (`encode_key_xi2_for_device`, XI2 forms in delivery); tests beside Task 1's.

**Interfaces:** Consumes Task 1's marker.

- [ ] **Step 1: Failing tests**
  - `xi2_repeat_has_flag_and_no_release`: per repeat one `XI_KeyPress` with `XIKeyRepeat` and no `XI_KeyRelease`;
    the first press and the real release have no `XIKeyRepeat`. Mutations: flags left 0; XI2 synthetic release kept.
  - `xi2_shadows_core_for_repeat`: a client selecting both XI2 and core gets only the XI2 form (today's rule), so no
    core synthetic release either.
  - `xi2_grab_owner_repeat`: an XI2 keyboard grab owner gets the same form.
- [ ] **Steps 2–4** as Task 1. **Step 5: Commit** `fix(xi2): mark repeated key presses with XIKeyRepeat (#355)`.

### Task 3: Status and gate

- [ ] **Step 1:** `docs/status.md`: one entry at the top for #355 (measured cause; what changes for flagged and XI2
  clients; unchanged for others; the accepted divergence: the skipped release keeps its server-side effects, and
  Xorg's single-press model is not adopted).
- [ ] **Step 2: Gate:** `cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked
  --features xdmcp`; the whole ignored suite `--workspace --locked --features xdmcp -- --ignored` (lavapipe env);
  `cargo clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp -- -D
  warnings`; FreeBSD clippy; `tools/check-layout.sh`; `just man`; the key/XKB/XI test modules 5 runs under 16 busy
  loops.
- [ ] **Step 3: Commit** `docs(status): detectable auto-repeat and XI2 key repeat (#355)`.

**Hardware check (coordinator, with the user's OK):** `repeat-probe` against the branch: flagged core = N presses and
1 release; unflagged core unchanged; XI2 = presses with `XIKeyRepeat`, 1 release. Then the user checks Marvel Rivals
(Tab) and MGSV.
