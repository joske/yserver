# RANDR CRTC model and hotplug relight — implementation plan

Design: [`2026-09-17-randr-crtc-model-and-hotplug-relight-design.md`](../specs/2026-09-17-randr-crtc-model-and-hotplug-relight-design.md).
Read it first; every decision below is argued there and is not repeated.

> **Status: not started.** Branch `fix/randr-crtc-model-hotplug-relight`, spec
> committed as `f8438636`. Design approved by codex after nine review rounds.

## Ordering principle

**P1 ships alone and first.** It closes the reported bug —
a monitor power-cycled while the session is awake never comes back — and it
needs none of P2 or P3. Everything after it is model work that no user is
currently blocked on. If P3 stalls, P1 must already be merged.

Two hard ordering constraints fall out of the design and drive the step
boundaries below:

1. **The relight and the reserved-slot policy are one commit.** Relighting
   without reservations reproduces codex's overlap: A at x=0, B at x=1920,
   unplug A, compaction moves B to x=0, replug A restores A at x=0, outputs
   overlap. A step that "will be fixed by the next one" here ships a corrupted
   desktop layout.
2. **The request-routing rewrite lands in the same commit as P3a-3.** P3a-3 is
   what turns `RandrOutput::crtc_id` from an identity into a binding, and the
   moment it does, `process_request.rs`'s `find(|o| o.crtc_id == crtc)` stops
   finding connected-but-Off outputs. Splitting them leaves a commit where
   `xrandr --output … --auto` on an idle output fails.

Per `feedback_no_commit_before_smoke`, every step that touches KMS or changes
what a client sees is **observed on hardware by jos before it is committed**.
Static checks do not catch a dark monitor.

## Prerequisites

- `aa2dff45` on master (current HEAD at branch point).
- No GH issue exists yet — the reporter was asked to open one on discussion #56
  and had not as of 2026-09-17. If one appears, reference it in the PR, and keep
  every step on this one branch per `feedback_one_branch_per_issue`.
- Gates for every step: `cargo +nightly fmt`, `cargo clippy --all-targets -- -D
  warnings` (CI's exact invocation — a crate-scoped run misses test-code lints),
  `cargo test`.

---

## Step 1 — move compaction and extent recompute out of the snapshot

Pure refactor, no behaviour change. `apply_connector_snapshot`
(`platform.rs:6620-6631`) stops calling `recompact_horizontal_layout` and stops
recomputing `fb_w`/`fb_h`; it reports the dropped routes' rectangles instead.
The backend caller does both, immediately, in the same order.

Doing this first means step 2 changes *policy* in a file where the mechanism
already moved, instead of doing both at once.

**Proof.** `cargo test` green. The decisive check is that this step is a no-op:
an unplug on hardware still compacts survivors and shrinks the extent exactly
as master does. If anything moves differently, the refactor is wrong, not the
policy.

## Step 2 — `last_enabled`, reserved slots, and the relight

**This is the fix.** One commit, per ordering constraint 1:

- `ConnectorEntry::last_enabled: Option<ConnectorConfig>`, written when `config`
  leaves `Enabled` because the connector physically departed; cleared by a
  client's explicit `SetCrtcConfig(mode=None)` and by an incompatible-mode
  reconnect.
- Dropped routes with a `last_enabled` reserve their `(x, y, w, h)`: excluded
  from packing, unioned into the extent.
- `run_display_rescan` gains the five-step order — snapshot, reconcile,
  **relight**, compact, publish.
- Relight restores **every** physically lost enabled route, not only
  `client_configured` ones.

Shape `last_enabled` so P3b can add a CRTC to it without a rewrite (design,
"P3 extends `last_enabled`").

**Unit tests.** Drop/re-add restores `Enabled`; drop/re-add after a client
disable does not; incompatible mode leaves it Off *and* clears `last_enabled`,
asserted across a **second** rescan so a stale reservation cannot re-reserve;
dropping a never-enabled output still compacts; and codex's overlap case —
A@0 + B@1920, drop A, assert B stays at 1920 and the extent stays 5120, re-add
A, assert no overlap.

**Hardware proof (jos, before commit).** The reported repro: awake XFCE, power
cycle monitor 2, no VT switch and no DPMS. The monitor returns with no client
action and zero `BadMatch` on minor 21 in the log. Then the original DPMS repro
end to end, and a VT-away power-cycle to confirm the path that already worked
still does.

**Merge P1 here.** Do not hold it behind P2/P3.

## Step 3 — `crtc_info` reports attached vs possible

Core-only; the wire encoder already takes independent `outputs` and `possible`
slices, so only `crtc_info` and its caller at `process_request.rs:3069` change
(that caller currently passes one `output_ids` array as both).

Idle CRTC ⇒ empty attached list, zeroed `x/y/width/height/mode_id`.

**Proof.** Unit test on an idle CRTC. On hardware, `xrandr --verbose` with one
output off no longer shows that output attached to its CRTC. xts RANDR purposes
show no PASS→FAIL against our own previous run in `docs/test-status.md` — never
against Xorg (`feedback_xts_iteration`).

---

P3 starts here. Each step below is invisible to clients except where marked.

## Step 4 — P3a-1: add `RandrState::crtcs`, populate it, read nothing

Additive. `RandrCrtc { crtc_id, mode_id, x, y, width, height, attached_outputs }`
— **no `DrmDeviceKey`, no `crtc::Handle`**; core depends only on
`yserver-protocol` and `DrmDeviceKey` is `pub(crate)` in `yserver`. Populate it
alongside the existing derivation. `screen_resources_current` and `crtc_info`
still use the old path.

`PlatformBackend` retains the per-device CRTC list from `ResourceHandles`, which
discovery currently drops after use.

**Proof.** Byte-identical replies — assert the new collection agrees with the
derived one for every CRTC, which is the cheapest possible check that step 5 is
safe.

## Step 5 — P3a-2: readers switch to the collection

`screen_resources_current` and `crtc_info` source from `RandrState::crtcs`.
Still 1:1, so output stays byte-identical; this is the step after which an
unpaired CRTC is *expressible*.

**Proof.** Same replies as step 4 on hardware. Unit test: a CRTC with no output
appears in `GetScreenResources` with an empty attached list.

## Step 6 — P3a-3: re-key CRTC XIDs, split `crtc_id`, rewrite request routing

**Protocol-visible: advertised CRTC ids change.** Identity migration only — no
new configuration is accepted. Three things, together, per ordering constraint 2:

- CRTC XIDs keyed on `(DrmDeviceKey, crtc::Handle)` in `RandrIdAllocator`;
  `ConnectorIds` loses `crtc_id`; `ids_for` stops minting one per connector.
  Add the **live-validated reverse lookup** (XID → `(device_key, handle)`
  ∩ current projection) — never a bare map inversion.
- `RandrOutput::crtc_id` becomes the current binding (0 = unbound) and
  `possible_crtc_ids` appears, still a singleton.
- **Request routing**: enable resolves the target from `outputs[]` (exactly one,
  no cloning); disable resolves an optional output from `attached_outputs` and
  treats an idle CRTC as successful no-op. `output_id`/`connector` become
  `Option` on the trait and in `CrtcConfigCompletion`; `requested_crtc` is
  always required. Nested and recording backends need compile-level updates.
- Validation: CRTC existence checked even for `mode = None`; `outputs.len() > 1`
  rejected with `BadMatch`; A→B move and disable post-states as specified.

Enumerate the `crtc_id` read sites before editing — several in `backend.rs` are
Present/pageflip CRTC handles in a different namespace and must not be touched.

**Proof.** Unit tests: enable an idle CRTC; disable attached; disable idle
(no-op success); `outputs.len() > 1` → `BadMatch`; unknown CRTC with
`mode = None` → `RANDR_BAD_CRTC`; A→B move with all four postconditions
including A's zeroed geometry; failed apply leaves A owning the output; disable
sets the output's `crtc_id` to 0; already-removed XID → `RANDR_BAD_CRTC`;
device removed between validation and apply → `RRSetConfigFailed` with no
substitution onto a same-numbered handle on a surviving device.

**Hardware (jos).** Full desktop bring-up on XFCE and one other WM, plus
`xrandr --output … --off` / `--auto` round-trips. This is the step where a
mistake makes outputs unaddressable.

## Step 7 — P3b: thread the requested CRTC, and remember it

- `apply_crtc_config` / `begin_crtc_config` gain `requested_crtc`.
- `PendingCrtcConfigProbe` gains `requested_crtc`; it already carries
  `prepared_output`.
- Connector preparation takes a pinned `crtc::Handle`; `connector_candidate`
  honours it and primary-plane selection filters to planes that can drive it.
  `enable_connector` and below need no signature change — `Output` already
  carries `crtc`.
- **Add and populate `ConnectorConfig::Enabled.crtc_id`, and snapshot it into
  `last_enabled`** (codex round 9). It belongs here, not in step 8: by now the
  bound CRTC is provably the requested one, and it must be recorded before a
  client can pick a non-default CRTC.

**Proof.** The CRTC bound is the one requested, asserted from the KMS side, not
inferred. Still no new configurations accepted, so hardware behaviour is
unchanged — which is itself the check.

## Step 8 — P3c: advertise usable route tuples

**The payoff.** `possible_crtc_ids` = `{ xid(c) : (e, c, p) usable }` over
encoder *and* primary-plane reachability, not encoder masks alone. Preparation
selects a whole tuple. Encoder retention is decided **per requested route** —
keep the current encoder only if it participates in a usable tuple for the
requested CRTC — not on a union property.

**Proof.** The synthetic unit test the hardware cannot produce: an encoder
reaching two CRTCs while the only primary plane reaches one, asserting the
unreachable CRTC is absent. Plus reconnect-after-A→B returns on B, and its
negative. On hardware, the original xfce `SetCrtcConfig crtc=0x12 … outputs=0x05`
succeeds.

## Hazards

- **The extent is computed in two places after step 1** until step 2 teaches it
  about reservations. Check no caller recomputes it from live layouts alone.
- **Step 6 changes advertised ids.** Clients caching CRTC ids across it need the
  `config_timestamp` bump; verify `rebuild_randr_state` advances it.
- **`possible clones` is one distinct bit per encoder on both silence devices**,
  so cloning cannot be exercised here at all. The `BadMatch` rejection is
  unit-testable only.
- **Steps 4-7 are individually unobservable**, which means a mistake in them
  surfaces only at step 8. The byte-identical-reply assertions in steps 4 and 5
  exist precisely to stop that.
- **`fix/randr-crtc-model-hotplug-relight` is a shared checkout** — do not
  `git checkout` other branches to compare (`feedback_dont_switch_branches_shared_worktree`);
  use `git show <ref>:<path>`.
