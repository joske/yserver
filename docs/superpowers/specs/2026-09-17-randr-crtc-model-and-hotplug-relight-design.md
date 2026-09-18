# RANDR CRTC model and hotplug relight

> **Status: superseded by measurement. Do not implement P1 or P3 from this
> document.** Reported by BergmannAtmet in
> GH discussion #56 (comment 18481108, 2026-09-17): after `xset dpms force
> suspend` → monitor off → monitor on, the session never renders again.
> Root-caused from a `yserver-xfce-hw-trace` run by jos the same day
> (`yserver-hw-xfce.log` 21 MB + `xfce.xtrace` 95 MB, not committed).
> jos's initial read was "awesome/picom specific"; the trace disproves that —
> it reproduces on XFCE and the mechanism is entirely in the KMS/RANDR path.
>
> The later Xorg/MATE trace and a plain-master hardware run refuted P1's
> auto-relight premise: a conforming desktop performs the disable, resize and
> re-enable itself. The independently correct P2 attached-vs-possible CRTC
> distinction was implemented instead and fixes XFCE's request shape. See
> [`2026-09-18-relight-premise-refuted-by-xorg-trace.md`](../findings/2026-09-18-relight-premise-refuted-by-xorg-trace.md).
> The broad physical-CRTC model (P3) is deferred until a separate reproducer
> requires it.
>
> **Reviewed by codex, 2026-09-17.** P1 and P2 approved with changes, both
> applied below: P1 gained an explicit ordering and now restores *every*
> physically lost enabled route rather than only `client_configured` ones.
> codex also corrected a claim in Defect 3 about the encoder mask being
> "already read"; the correction is inline and carried into P3c.
>
> **Round 2** fixed a P1 layout blocker (compaction ran before the relight and
> produced overlapping outputs) with the reserved-slot policy. **Round 3** fixed
> two more: the extent recompute cannot live in `apply_connector_snapshot`, and
> releasing a reservation must clear `last_enabled`.
>
> **P3 was sent back twice as "design work, not ready to plan"; it is now
> designed** (P3a–P3e), on top of a `modetest` measurement of the real CRTC and
> encoder topology. **Round 4** fixed a dependency inversion in the proposed
> core type, specified the atomic A→B move and CRTC-existence check that the
> 1:1 model had made unrepresentable, and separated P3e's identity migration
> from its capability change. **Round 5** fixed the request-routing rule — the
> core resolved the backend target by searching for an output already bound to
> the addressed CRTC, so enabling an *idle* CRTC (the reported case) died before
> KMS saw it — plus three documentation contradictions. P1 is approved; P3
> awaits re-review. **Round 6** added the live-validated XID→`crtc::Handle`
> reverse lookup and redefined `possible_crtc_ids` over usable
> (encoder, CRTC, primary-plane) tuples rather than encoder masks alone.
> **Round 7** narrowed P3c's encoder-skip condition from a global union property
> to a per-requested-route test, and split the reverse-lookup proof in two.
> **Round 8** extended `last_enabled` with the assigned CRTC — P1's remembered
> state was designed under the 1:1 model and would have silently re-routed a
> moved output across a replug once P3 landed. **Round 9** named the
> `ConnectorConfig::Enabled.crtc_id` step explicitly in P3e's commit order.
>
> **codex approved the design for planning after round 9.** The implementation
> plan is the next artifact; nothing here is implemented.

## Problem

A monitor that is power-cycled while the session is awake never comes back.
The connector is dropped on the disconnect edge and never re-enabled on the
reconnect edge, and the desktop's own attempt to re-enable it is rejected
with `BadMatch` forever.

### The measured chain

From `yserver-hw-xfce.log`:

```
13:45:49  render rescan: output HDMI-3 disconnected — dropping active scanout
13:45:49  kms: RandR output disconnected: HDMI-3 on 226:2
13:45:55  kms: RandR output connected: HDMI-3 on 226:2     <- logged, nothing enabled
13:45:56  client 8 #959 RANDR::SetCrtcTransform is unsupported
13:45:56  emit_x11_error: client=8 seq=961 code=8 bad_value=0x12 minor=21 major_opcode=128
```

`minor=21` is `RRSetCrtcConfig`; `code=8` is `BadMatch`. xfsettingsd retries at
13:46:06, :08, :09, :15, :16, :16, :17 and :36 — byte-identical request, same
error each time.

From `xfce.xtrace`, our replies one second earlier and the request they produced:

```
GetOutputInfo 0x05  Connected    crtc=0x0  crtcs=0x06; modes=0x13,…  name='HDMI-3'
GetOutputInfo 0x11  Disconnected crtc=0x0  crtcs=0x12; modes=;       name='DP-2'
GetCrtcInfo   0x12  mode=0  outputs=0x11;  possible outputs=0x11;
SetScreenSize 5120x1440                                    (accepted)
SetCrtcConfig crtc=0x12 mode=0x13 outputs=0x11,0x05   ->  Error 8=Match bad=0x12
```

RANDR resource ids are **stable** across the replug — HDMI-3 is output `0x05`
on crtc `0x06` for the whole run — so id churn is not involved. `SetScreenSize`
is accepted, so screen-extent growth is not involved either. Only
`SetCrtcConfig` fails.

### Defect 1 — the reconnect never relights (regression, `a6f8909c`)

`a6f8909c` "Support reverse-PRIME (#95)", 2026-08-25, deleted
`PlatformBackend::requery_outputs_and_modeset`. Its doc comment at
`a6f8909c^:crates/yserver/src/kms/render/platform.rs:3125` read:

> Re-scan connectors on the existing device, dropping missing outputs,
> refreshing surviving output metadata, and **adding newly connected outputs**.

It had two callers, `run_display_rescan` and `run_resume`, and both lost
re-discovery. The hotplug rescan now uses `probe_connector_snapshot` +
`apply_connector_snapshot`, which carries a deliberate new policy at
`crates/yserver/src/kms/render/platform.rs:6615`:

```rust
// Runtime discovery never auto-enables a newly-connected connector.
// It enters the registry connected-but-Off; SetCrtcConfig performs
// the expensive assignment/scanout allocation if a client enables it.
```

`rescan.added_keys` is consumed at `crates/yserver/src/kms/render/backend.rs:11859`
by a `log::info!` and nothing else. The **drop** side was kept in full
(`platform.rs:6556` removes the output layout, the scanout pool, the BO
generations, the first-pageflip ledger).

With no live output left, the wake path is a permanent no-op:
`set_dpms_power(0)` runs `dpms_set_outputs_active(true)` over an empty vector,
gets `Ok(())`, and then `backend.rs:26623` recomputes
`kms_outputs_active = !self.platform.outputs.is_empty()` — still false. Every
subsequent `maybe_composite` returns at `backend.rs:17992`.

The **VT path is not affected and was measured working** (jos, 2026-09-17: xfce
on TTY3 → TTY4 → power-cycle → back → monitor returns). `run_display_rescan`
bails at `backend.rs:11998` when `vt_state != Active`, so while VT-away nothing
is torn down and `run_resume`'s re-commit of the still-present output succeeds.
The two scenarios differ by exactly one variable: whether the rescan runs.

### Defect 2 — `GetCrtcInfo` invents an attachment on an idle CRTC (pre-dates #95)

`crates/yserver-core/src/randr.rs:721`:

```rust
pub fn crtc_info(&self, crtc_id: u32, config_timestamp: u32) -> Option<CrtcInfoData> {
    let out = self.outputs.iter().find(|o| o.crtc_id == crtc_id)?;
```

The 1:1-paired output is reported unconditionally, as both `outputs` and
`possible outputs`, even when it is disconnected with `mode_id == 0`. That is
why the trace shows an idle CRTC `0x12` claiming to drive a disconnected DP-2,
and why xfce carried DP-2 into the output list of its request.

Xorg reports the **currently attached** set — `rep.nOutput = crtc->numOutputs`,
`../xserver/randr/rrcrtc.c:1221` — which is 0 for an unused CRTC. On Xorg xfce
would have sent `outputs=0x05` alone.

### Defect 3 — the output↔CRTC model is rigidly 1:1 (pre-dates #95)

`RandrIdAllocator::ids_for` (`crates/yserver/src/kms/render/backend.rs:996`)
mints a **synthetic CRTC XID per connector**:

```rust
let ids = ConnectorIds { output_id: self.fresh(), crtc_id: self.fresh() };
```

That is the `0x03/0x04`, `0x05/0x06`, … `0x11/0x12` pairing visible in the
trace. Our RANDR CRTC namespace has no relationship to the kernel's CRTC set:
we advertise one CRTC per connector ever seen, where the hardware has a fixed
smaller pool that any connector can borrow.

The consequences are advertised all the way out:

| site | current | should be |
|---|---|---|
| `randr.rs:620` | `possible_crtcs: vec![out.crtc_id]` | every CRTC reachable by a usable (encoder, CRTC, plane) route — see P3c |
| `randr.rs:721` | one paired output, always | attached set (may be empty) + possible set |
| `randr.rs:42` doc | "1 connector, 1 CRTC, 1 mode in the current model" | CRTCs are a shared pool |

So `validate_set_crtc_config`'s `if out.crtc_id != crtc_id`
(`crates/yserver-core/src/randr.rs:658`) returns `BadMatch` with
`errorValue = crtc_id = 0x12` — exactly the wire bytes above. A client doing
ordinary CRTC bookkeeping has zero freedom on us and any choice but our invented
partner is a hard error.

A first draft of this spec claimed the kernel data we need is "already read,
just never surfaced". **That was wrong** (caught by codex). `drm/modeset.rs:401`
picks a *single* encoder:

```rust
let encoder_handle = info
    .current_encoder()
    .or_else(|| info.encoders().first().copied())
```

and then masks with only that encoder's `possible_crtcs()` at `:409`. A real
RANDR `crtcs[]` is the usable **union across the connector's encoder routes**,
and choosing a CRTC reachable only through a different encoder must select that
encoder too. That is new work, not a plumbing change — see P3c.

### Attribution

Defects 2 and 3 are from `6c8e8db6` "feat(phase6-10): multi-monitor on KMS",
2026-05-07 — months before #95. They were **dormant** because the server relit
the returning connector itself and no client ever had to configure an output.
`a6f8909c` shifted recovery onto the client, whose only route is
`SetCrtcConfig`, which our model rejects. One regression turned two latent
modelling bugs into a hard failure.

## Xorg reference behaviour

Per AGENTS.md ("if Xorg deviates from spec, we follow Xorg"):

- **Disconnect does not tear down the CRTC.** `drmmode_update_kms_state`
  (`../xserver/hw/xfree86/drivers/modesetting/drmmode_display.c:4197`) re-detects
  outputs, fires RANDR events, and on a `link-status == BAD` connector actively
  **re-sets the current mode** to keep the display alive. The CRTC assignment
  survives an unplug; only a client reconfigures routing.
- **`GetCrtcInfo` distinguishes attached from possible.**
  `rep.nOutput = crtc->numOutputs` (`rrcrtc.c:1221`); `nPossibleOutput` counts
  every non-leased output whose `crtcs[]` contains this CRTC (`rrcrtc.c:1223-1231`).
- **CRTCs are a shared pool.** Each output's `crtcs[]` is the driver's real
  possible set, so a client may legally bind any reachable CRTC.

## Design

### P1 — restore the reconnect relight

Closes the reported bug on its own. Two candidate shapes; **P1b is proposed**.

**P1a — Xorg-style, never tear down.** Keep the output in `platform.outputs`
across a disconnect, flag it not-connected, keep its scanout pool. Closest to
Xorg. Rejected as the primary: it pins scanout BOs, per-device descriptor pools
and `bo_generations` to a connector that may never return, which is precisely
the resource ownership #95 restructured away from, and it would have to hold
them across an arbitrary multi-GPU topology change.

**P1b — remember and re-enable (proposed).** Keep #95's teardown. Persist the
route policy of a connector retired by a *physical* disconnect, and re-apply it
when the same key returns with a compatible mode.

Today that policy is destroyed on the drop —
`crates/yserver/src/kms/render/backend.rs:7990`:

```rust
entry.connected = false;
entry.config = ConnectorConfig::Off;
entry.client_configured = false;
```

`ConnectorConfig::Enabled { mode_w, mode_h, vrefresh, x, y }` holds everything
the relight needs **while the model is 1:1**. It does not once P3 lands — the
selected CRTC becomes a real degree of freedom and must be remembered too; see
"P3 extends `last_enabled`" under P3. Anyone implementing P1 alone should expect
that field to gain a CRTC, and shape the struct so adding it is not a rewrite.

P1b:

1. Add `ConnectorEntry::last_enabled: Option<ConnectorConfig>`, written
   whenever `config` transitions away from `Enabled` *because the connector
   physically departed* — not when a client disables it. A client's explicit
   `SetCrtcConfig(mode=None)` must clear `last_enabled`, or unplugging a
   deliberately-disabled monitor would resurrect it.
2. On reconnect, restore **every** route lost to a physical disconnect, whether
   or not `client_configured` was set. *(codex, 2026-09-17: gating on
   `client_configured` would leave boot auto-layout sessions — no display
   daemon, nothing ever called `SetCrtcConfig` — with exactly the reported
   failure. That is the reporter's own configuration.)* `client_configured`
   still governs whether the auto-layout may later move the output; it must not
   govern whether the output exists.
3. If no compatible mode survives (monitor replaced by a different panel),
   leave it connected-but-Off and let the desktop decide. Log the reason.

**Ordering inside `run_display_rescan` is load-bearing** (codex). The relight
must sit between registry refresh and publication, so that the scene rebuild and
the RANDR notifications describe the *post*-relight topology and clients never
observe the intermediate output-less state:

1. `apply_connector_snapshot` — apply the physical snapshot. **No compaction and
   no extent recompute** (see "Layout policy" below); it reports which routes it
   dropped and the backend records their rectangles as reservations.
2. `reconcile_connector_registry` — refresh connected bits, mode lists, EDID.
3. **Relight**: for each returning key with a `last_enabled` whose refreshed
   mode list still contains a mode matching `(mode_w, mode_h, vrefresh)`, drive
   the existing `enable_connector(&output_key, output, mode_spec, x, y)` path
   (`backend.rs:19646`) — the same one `SetCrtcConfig` uses, so pool allocation,
   modeset, `ActiveOutput` update and fb-extent recompute are already handled.
4. **Compact** auto-layout outputs, skipping `client_configured` and reserved
   slots, then recompute the extent over live layouts ∪ surviving reservations.
5. `fire_randr_changes` — scene rebuild, `rebuild_randr_state`,
   `kms_outputs_active` reconciliation, change notifications.

Auto-relight must **not** set `client_configured` — it is restoring a previous
state, not recording a new client intent.

#### Layout policy — reserved slots (resolved, was an open question)

Ordering alone is not enough. **Compaction already happens inside step 1**, at
`crates/yserver/src/kms/render/platform.rs:6621`, the moment an output is
dropped:

```rust
if !rescan.dropped_old_indices.is_empty() {
    self.recompact_horizontal_layout(client_configured);
    …
    let (fb_w, fb_h) = recompute_fb_extent_from(&layouts);   // survivors only
```

codex's counterexample: A at x=0, B at x=1920. Unplug A — compaction moves B to
x=0 and the extent shrinks. Replug A — P1 restores A's remembered x=0. **The two
outputs now overlap.** P1 would restore scanout while corrupting the desktop
layout, which is worse than the bug it fixes. Neither is `client_configured` in
a bare session, so the skip in `recompact_horizontal_layout` does not save us.
The trace shows the same shrink live: xfsettingsd `SetScreenSize 2560x1440` on
the drop, `5120x1440` on every retry after.

**Policy.** A route that is physically gone but restorable keeps its slot:

1. **Compaction *and* the extent recompute move out of
   `apply_connector_snapshot`.** Both become explicit steps the caller runs, so
   the snapshot decides topology ownership only, never layout policy.

   *(codex, round 3: an earlier draft left the extent recompute in the snapshot.
   That cannot work — `self.outputs.remove(idx)` at
   `crates/yserver/src/kms/render/platform.rs:6584` has already deleted the
   dropped layout by the time `recompute_fb_extent_from` runs at `:6621`, so the
   reservation rectangle has no owner at the point it is needed. The alternative
   — passing precomputed reservation rectangles *into* the snapshot — was
   rejected: it pushes layout policy back down into the topology layer. The
   backend is the only place that knows which routes are restorable, so it
   decides the extent, once, after registry reconciliation.)*

2. **A dropped route with a `last_enabled` reserves its `(x, y, width,
   height)`.** Reserved slots are excluded from the packing range, so surviving
   auto-layout outputs do not move into the hole, and they are unioned with the
   live layouts when the backend computes the extent, so the virtual extent does
   not shrink.
3. **Compaction runs after the relight decision**, over auto-layout outputs only
   — not `client_configured`, not reserved. The extent recompute follows it,
   over live layouts ∪ surviving reservations.
4. **Reservation is released *by clearing `last_enabled`*** when the output is
   no longer restorable: a client `SetCrtcConfig` on it (enable elsewhere or
   disable), or a reconnect whose refreshed mode list contains no match for the
   remembered mode.

   *(codex, round 3: reservation eligibility is derived from `last_enabled`, so
   dropping a separate reservation record while leaving `last_enabled` set would
   let the same stale route re-reserve the hole on the very next rescan. There
   is exactly one piece of state; clearing it is the release. The alternative —
   a persistent explicit "not restorable" flag alongside `last_enabled` — is
   redundant with clearing it.)*

This is the P1b equivalent of Xorg retaining the CRTC route without retaining
the BOs (codex), and it is the *more* Xorg-faithful choice in both halves:
`drmmode_update_kms_state` never repacks a layout and never resizes the screen
on a disconnect — our compaction is a yserver invention for sessions with no
display daemon. Keeping the extent stable across an unplug/replug also removes
the `SetScreenSize` churn the trace shows.

**Accepted cost:** a monitor that never returns leaves a gap and an oversized
screen until something reconfigures. That is exactly what stock Xorg does, and
any display daemon corrects it on the next `SetCrtcConfig`. Flagged for jos
rather than hidden: the alternative — compacting immediately and re-expanding on
return — is the behaviour that produced the overlap above.

### P2 — make `crtc_info` tell the truth

Split `CrtcInfoData`'s single `output_id` into two vectors:

- `outputs`: the CRTC's currently attached outputs. Empty when the CRTC is idle
  (`mode_id == 0`), which is the whole fix for the phantom.
- `possible_outputs`: every output whose possible-CRTC set contains this CRTC.

Under the P3 model those differ; under the current 1:1 model `possible_outputs`
stays a singleton and only `outputs` changes. **P2 is therefore mergeable before
P3** and is load-bearing for it: without P2 a client keeps merging phantom
outputs into its request and P3's validation would reject it on different
grounds.

`x`, `y`, `width`, `height`, `mode_id` for an idle CRTC must read 0, matching
`rrcrtc.c:1193-1198`.

**Boundary.** P2 works standalone only because under the 1:1 model every CRTC
still has a paired output to look up, so `crtc_info` can report
`outputs = []` + `possible_outputs = [paired]` without a CRTC object. The CRTC
*list* stays output-derived (`randr.rs:540`) until P3a. P2 therefore fixes the
phantom attachment but cannot yet express a CRTC that no output is paired with.

### P3 — real many-to-many CRTCs

codex rejected the first draft of this phase, which assumed the change was
mostly "publish a wider `possible_crtcs`". Two structural blockers made it a
model change rather than a plumbing change, and a third was the encoder
correction from Defect 3. All three are designed below, and the topology they
depend on has been measured rather than assumed.

The one question that had no Xorg precedent — what an async apply does when a
racing hotplug takes the requested CRTC — turned out to have a *spec* answer
rather than needing an invented policy; see P3b. P3e gives the migration order,
because `RandrOutput::crtc_id` changes meaning and that is a writers + readers +
field flip.

#### Measured topology (silence, 2026-09-17)

`modetest -e` / `-c` on the box the xfce trace came from — CRTC handles 484 and
489 match the log, so this is the same hardware:

| device | CRTCs | encoder `possible crtcs` | connector→encoder | `possible clones` |
|---|---|---|---|---|
| amdgpu 3.64.0 (RX 6800) | **6** | `0x3f` on *every* encoder | 1:1 (DP-3→510, DP-4→519, DP-5→525, HDMI-A-3→532), plus MST/Virtual | one distinct bit each |
| i915 | **4** | `0x0f` on *every* encoder | 1:1, plus MST | one distinct bit each |

Planes on the RX 6800 (`modetest -p`) — note these are **not** uniform like the
encoder masks, which is why P3c must reason about route tuples:

| type | ids | `possible crtcs` |
|---|---|---|
| Primary | 44, 98, 152, 206, 260, 314 | `0x20, 0x10, 0x08, 0x04, 0x02, 0x01` — one CRTC each, covering all six |
| Overlay | 368, 424 | `0xff` |
| Cursor | 480, 485, 490, 495, 500, 505 | one CRTC each |

Three things this settles:

1. **Every CRTC is reachable from every connector on both drivers.** So the
   correct `crtcs[]` for an output is the device's whole CRTC set, and the
   xfce request that started this — HDMI-3 on CRTC `0x12` — was legal on the
   hardware all along. Our `BadMatch` was purely our own invention.
2. **No CRTC is reachable only through another encoder.** The hard half of
   P3c — having to *switch encoders* to honour a CRTC choice — does not arise
   on this hardware. P3c still computes a union for correctness elsewhere and
   for MST, but it is a union of one here.
3. **The hardware refuses cloning anyway.** Each encoder's `possible clones` is
   its own single bit, so P3d's "cloning out of scope" costs nothing on these
   devices.

#### P3a — an explicit CRTC object

**What was wrong.** There is nowhere to put a CRTC that no output is paired with:
`crates/yserver-core/src/randr.rs:540` derives the advertised CRTC list from
outputs (`self.outputs.iter().map(|o| o.crtc_id)`), and `:721` finds a CRTC *by*
its paired output. An unpaired kernel CRTC is inexpressible.

**Core model.** `RandrState` gains a CRTC collection and stops deriving one:

```rust
pub struct RandrCrtc {
    pub crtc_id: u32,               // stable XID — already qualified by the backend
    pub mode_id: u32,               // 0 when idle
    pub x: i16, pub y: i16,
    pub width: u16, pub height: u16,
    pub attached_outputs: Vec<u32>, // empty when idle
}

pub struct RandrState {
    …
    pub crtcs: Vec<RandrCrtc>,      // NEW — the source of truth
}
```

**No device identity in core** (codex, round 4). An earlier draft of this type
carried `device_key: DrmDeviceKey` and a raw `crtc_handle`. That is a dependency
inversion: `yserver-core` depends only on `yserver-protocol`, while `yserver`
depends on `yserver-core` — and `DrmDeviceKey` is `pub(crate)` in the *yserver*
crate at `crates/yserver/src/platform/drm.rs:35`, so core cannot name it at all.

Core does not need it. To serialize RANDR it needs already-qualified CRTC XIDs
and the output↔CRTC relationships; which card owns a CRTC is a backend concern.
Device identity stays in the KMS projection — the allocator's
`(DrmDeviceKey, crtc::Handle) → XID` map — and **per-device grouping is already
expressible in core** through the RANDR 1.4 provider list, which carries
`RandrProvider { crtcs: Vec<u32>, outputs: Vec<u32>, … }`. No new opaque key is
needed; the existing mechanism covers it.

Consequence for invariant 5 (no output lists a foreign CRTC): it is enforced and
tested **backend-side**, where `possible_crtc_ids` is computed, because core has
no way to tell two devices apart.

`RandrOutput`'s single `crtc_id` splits:

| field | meaning | `GetOutputInfo` |
|---|---|---|
| `crtc_id: u32` | **current binding**, 0 when unbound | `crtc` |
| `possible_crtc_ids: Vec<u32>` | **capability**, from P3c | `crtcs[]` |

`output_info` (`randr.rs:620`) then returns `possible_crtcs:
out.possible_crtc_ids.clone()` instead of `vec![out.crtc_id]`, and its comment
"Our model is a stable 1:1 output↔crtc allocation" goes with it.
`screen_resources_current` sources `crtcs` from `state.randr.crtcs`.
`crtc_info` looks up by XID in the collection instead of scanning outputs, and
returns `attached_outputs` + a computed possible set.

**XID allocation.** `RandrIdAllocator::ids_for`
(`crates/yserver/src/kms/render/backend.rs:996`) stops minting a CRTC id per
connector. It gains a parallel map keyed the same stable way connectors are:

```rust
crtcs: HashMap<(DrmDeviceKey, crtc::Handle), u32>,
```

`ConnectorIds` loses `crtc_id`. `from_outputs`' doc comment — "outputs `1..=N`,
CRTCs `(N+1)..=2N`" — is the 1:1 assumption written down, and retires with it;
the two id spaces become independent, both still drawn from `fresh()`.

**Backend.** `rebuild_randr_state` must build `crtcs` from the real per-device
CRTC list. Discovery already reads `ResourceHandles` (it is threaded into
`connector_candidate` as `resources: &ResourceHandles` and used for
`filter_crtcs`); P3a needs that CRTC list *retained* per device in
`PlatformBackend` rather than dropped after discovery.

**No protocol change.** `encode_get_crtc_info_reply`
(`crates/yserver-protocol/src/x11/randr.rs:886`) already takes independent
`outputs` and `possible` slices. The phantom is fabricated entirely in the
caller, `crates/yserver-core/src/core_loop/process_request.rs:3069`:

```rust
let output_ids = [crtc_data.output_id];
…  outputs: &output_ids, possible: &output_ids,
```

**The wire encoder is already correct**, so neither P2 nor P3a needs a protocol
change. P2 is core-only. **P3a is not** (codex, round 5): the same section
requires `RandrIdAllocator` to re-key CRTC XIDs and `PlatformBackend` to retain
the per-device CRTC list from discovery. P3a is a core-model change *plus* a
backend-projection change; only the `yserver-protocol` crate is untouched.

#### P3b — thread the requested CRTC to KMS

**What was wrong.** The core validates the client's CRTC and then throws it
away, keeping it only as a lookup key for the output row
(`process_request.rs`: `find(|o| o.crtc_id == crtc)`). `trait_def.rs:627` and
`:646` take `(output_id, connector, mode, x, y)` — no CRTC — so yserver could
advertise a CRTC as valid, accept it, and silently bind another.

**Smaller than it first looked.** `drm::modeset::Output`
(`crates/yserver/src/drm/modeset.rs:179`) *already carries* `crtc`, `encoder`
and `plane`, and `enable_connector` takes a fully-built `Output` by value. So
`enable_connector` → `…_with_cursor_factory` → `…_inner` → `commit_modeset`
need **no signature change**: they already honour whatever CRTC the `Output`
names. The threading is three edits, not six layers:

1. **Trait** — `apply_crtc_config` and `begin_crtc_config` gain
   `requested_crtc: u32`, and `output_id`/`connector` become optional (see
   "Request routing" below). The default `begin_crtc_config` forwards them
   unchanged, so nested/recording backends keep their *behaviour* — but their
   implementations and tests still need a compile-level update for the new
   signature. Not a no-op edit.
2. **Pending state** — `PendingCrtcConfigProbe`
   (`crates/yserver/src/kms/render/backend.rs:861`) gains `requested_crtc: u32`.
   It already holds `prepared_output: Option<Output>`, which is exactly where
   the choice lands.
3. **Preparation** — the connector-preparation path that builds `Output` takes
   an optional pinned `crtc::Handle`. `connector_candidate`
   (`drm/modeset.rs:395`) selects that CRTC instead of its own first choice,
   and primary-plane selection filters `primary_planes: &[(plane::Handle,
   HashSet<crtc::Handle>)]` to planes whose set contains the pinned CRTC.

**Contention has a spec answer.** Because the async path defers the reply until
`finish_crtc_config`, a CRTC that was free at validation and taken by the time
preparation runs does not need an invented policy: re-validate at preparation
and return **`RRSetConfigFailed = 3`**
(`/usr/include/X11/extensions/randr.h:156`) in the `SetCrtcConfig` status byte.
That is a status, not an X error — the reply already carries one
(`randr.rs:1109`, and the trace shows `status=Success(0x00)`). Xorg being
synchronous simply never reaches this state; the status code it defines covers
it exactly.

#### Reverse mapping — XID → live DRM CRTC handle

`requested_crtc` is an X11 resource id; pinned KMS preparation needs a
`crtc::Handle`. P3a specifies only the forward direction,
`(DrmDeviceKey, crtc::Handle) → XID`. The backend needs the reverse, and it must
be **live-validated**, not a plain map inversion (codex, round 6):

```
requested_crtc (XID)  →  (device_key, crtc::Handle)
                         ∩ the currently projected topology
```

The allocator deliberately retains entries so ids stay stable across
disconnect/reconnect, so inverting it alone would happily hand back a handle on
a **GPU that is no longer present** — reachable in the multi-device world #95
introduced, and it would be applied to whatever device now answers. Intersecting
with the live projection is what prevents that.

Two distinct failure modes, two distinct answers:

| condition | who rejects | result |
|---|---|---|
| XID absent from `RandrState::crtcs` | core, "test 0" | `RANDR_BAD_CRTC` |
| XID valid at validation, not in the live projection at apply | backend reverse lookup | `RRSetConfigFailed` (3) |

The second is the same answer as async contention in P3b, and for the same
reason: the id *was* valid when the client asked, so it is a failure to carry
out a well-formed request, not a bad resource.

**Two tests, one per row** — a single combined test would pass while conflating
the two rejection paths:

1. **Already-removed XID.** The device is gone before the request arrives; the
   XID is absent from `RandrState::crtcs`. Assert core rejects it with
   `RANDR_BAD_CRTC` — the backend is never reached.
2. **Removed between validation and apply.** The XID is present and valid when
   `validate_set_crtc_config` runs; the device disappears before the async apply
   resolves it. Assert `RRSetConfigFailed`, **and** that no substitution
   occurred — in particular that it was not resolved onto a surviving device's
   CRTC carrying the same raw handle number, which is the failure a bare map
   inversion produces.

#### P3c — usable route tuples and per-route encoder selection

Per the Defect 3 correction, `crtcs[]` is the usable union across the
connector's encoder routes, not one encoder's mask. `connector_candidate`
(`drm/modeset.rs:401`) commits to `current_encoder().or(first)` before any mask
is read.

**But an encoder mask is not sufficient** (codex, round 6). A route is only
usable if a *primary plane* can also drive that CRTC. P3b discovers this during
preparation, which is too late: advertising a CRTC that core then accepts and
preparation then cannot serve is an invariant-3 violation with extra steps. The
advertised set must be derived from complete route tuples:

```
routes(connector) = { (e, c, p) :
    e ∈ info.encoders(),
    c ∈ filter_crtcs(get_encoder(e).possible_crtcs()),
    p ∈ primary_plane_candidates, c ∈ p.drivable }

possible_crtc_ids = { xid(c) : (_, c, _) ∈ routes(connector) }
```

`primary_plane_candidates` (`drm/modeset.rs:369`) already produces exactly the
`(plane::Handle, HashSet<crtc::Handle>)` pairs this needs; P3c consumes them at
*advertisement* time rather than only at preparation time. P3b's preparation
then selects a whole tuple — encoder, CRTC **and** plane — so the plane it binds
is one the advertisement already accounted for.

When a CRTC is pinned, select an encoder whose mask contains it, preferring
`current_encoder()` when it qualifies, so the common case commits no encoder
change.

**The skip condition is per requested route, not a global property** (codex,
round 7). An earlier draft said to skip encoder re-selection "when the union
equals the full device CRTC set". That is too broad: a *union* can cover every
CRTC while the *currently selected* encoder covers only some of them, so a
requested CRTC may still need a different encoder. Correct rule, evaluated per
request:

- retain the current encoder **only if it participates in a usable
  `(encoder, requested CRTC, primary plane)` tuple**;
- otherwise select another tuple, taking its encoder *and* its plane together.

**Per the measurement, encoder re-selection is unreachable on this hardware** —
and the condition that makes it so is the stronger one: on both silence devices
**each individual encoder** has the full mask (`0x3f` on amdgpu, `0x0f` on
i915), not merely their union. Every encoder reaches every CRTC, so the current
encoder always participates in a usable tuple and the retain branch always
fires. Write the re-selection correctly for MST and for display blocks with
genuinely partitioned masks, but **do not let it block P3a/P3b**.

**The plane half is different and must not be assumed away.** `modetest -p` on
the RX 6800 shows six primary planes whose `possible crtcs` masks are
`0x20, 0x10, 0x08, 0x04, 0x02, 0x01` — each drives **exactly one** CRTC, unlike
the encoders' uniform `0x3f`. The six happen to cover all six CRTCs bijectively,
so the tuple set still spans every CRTC here and the advertised set does not
shrink. That is a property of this card, not a guarantee: nothing makes the
plane count equal the CRTC count, and a card with fewer primary planes than
CRTCs would advertise a CRTC no plane can serve.

So the tuple formulation is load-bearing even though it changes nothing on
silence. Its unit test must be **synthetic** — this hardware cannot produce the
failing case: an encoder reaching two CRTCs while the only available primary
plane reaches one, asserting the unreachable CRTC is absent from
`possible_crtc_ids`.

Multi-GPU: an output on device A must never list a CRTC on device B. The
**backend projection** carries `device_key` (via `OutputKey` and the allocator's
`(DrmDeviceKey, crtc::Handle)` map), so the filter is applied there —
`RandrCrtc` must not carry it, per P3a. It needs an explicit test, because
nothing in the type
system enforces it.

#### P3d — validation, allocation and scope

`validate_set_crtc_config` (`crates/yserver-core/src/randr.rs:637`) changes its
middle test. Today:

```rust
if out.crtc_id != crtc_id { return Err((BAD_MATCH, crtc_id)); }
```

becomes, per output named in the request:

1. the output exists — `BadMatch(output_id)`, unchanged;
2. `crtc_id` ∈ `out.possible_crtc_ids` — else `BadMatch(crtc_id)`;
3. the CRTC is free, or already bound to exactly the named output set — else
   `BadMatch(crtc_id)`;
4. the mode is in the output's list — `BadMatch(mode_id)`, unchanged.

Xorg's field-for-`errorValue` choices are preserved throughout; only test 2/3
replace the identity check.

**Test 0 — the CRTC must exist, including for `mode = None`** (codex, round 4).
Today `validate_set_crtc_config` returns `Ok(None)` for the disable case
*before* establishing that `crtc_id` names anything:

```rust
if mode_id == 0 {
    if !outputs.is_empty() { return Err((BAD_MATCH, crtc_id)); }
    return Ok(None); // disable — crtc existence never checked
}
```

and there is no `RANDR_BAD_CRTC` pre-check in the `RR_SET_CRTC_CONFIG` arm of
`process_request.rs` either. Under the 1:1 model the downstream
`find(|o| o.crtc_id == crtc)` masked it. With a real CRTC collection, existence
must be established first and an unknown CRTC rejected with `RANDR_BAD_CRTC`
regardless of `mode`.

#### Request routing — resolve the target from `outputs[]`, not from the CRTC

**Without this, P3 rejects the very request it exists to support** (codex,
round 5). The core resolves the backend target by searching for an output
*already bound to* the addressed CRTC, at
`crates/yserver-core/src/core_loop/process_request.rs:4632`:

```rust
// Resolve connector name from crtc_id (validated above →
// guaranteed to exist).
let output_row = state.randr.outputs.iter().find(|o| o.crtc_id == crtc);
let Some(output_row) = output_row else {
    return emit_x11_error_with_minor(…, BAD_MATCH, crtc, …);
};
```

The comment's "guaranteed to exist" is itself a 1:1 artifact. Under P3 the
interesting case is an **idle** target CRTC, where that search returns nothing
and the request dies with `BadMatch(crtc)` before `begin_crtc_config` is
reached — exactly the xfce request this spec is about.

Replacement, by case:

| case | target resolution |
|---|---|
| **enable** (`mode != 0`) | resolve output id and connector name from the request's `outputs[]`, never from the CRTC. Cloning is out of scope, so require **exactly one** entry. |
| **disable** (`mode == 0`) | resolve the *optional* current output from the addressed `RandrCrtc::attached_outputs`. An **idle CRTC is a successful no-op** and needs no connector at all. |

Consequences:

- **Backend API**: `output_id` and `connector` become optional — a disable of an
  idle CRTC has neither — while `requested_crtc` is always required. This is why
  P3b's trait change is `Option<…>` rather than a straight addition.
- `CrtcConfigCompletion { output_id, … }` (same function) must carry the same
  optionality, or the completion path re-introduces the assumption.
- **Core validation gains an explicit no-cloning rule**: reject
  `outputs.len() > 1` with `BadMatch`. Today the 1:1 walk rejects multi-output
  requests as a side effect; P3d requires it be rejected *because we checked*.

Tests: enabling an **idle** CRTC (the reported case); disabling an **attached**
CRTC; disabling an **idle** CRTC (no-op success, not an error); and
`outputs.len() > 1` rejected with `BadMatch`.

#### The A→B move must be atomic

`SetCrtcConfig` can move an output between CRTCs, which the 1:1 model made
unrepresentable and the spec therefore never defined. On a successful apply, all
four of these happen together or none do:

1. remove the output from A's `attached_outputs`;
2. add it to B's;
3. set the output's `crtc_id` to B;
4. if A is now unattached, zero its `mode_id`, `x`, `y`, `width`, `height`.

Skip any one and the collection can claim a single output on two CRTCs, which
`GetCrtcInfo` would then publish. Step 4 is what makes "A became idle" true, and
is the precondition for P2's empty-attached-list behaviour to mean anything.

**The async failure path must leave both the old binding and the projected state
unchanged.** A `begin_crtc_config` that reaches `finish_crtc_config` with an
error — including the `RRSetConfigFailed` contention case above — must not have
partially applied the move. The projection is rebuilt from the backend's live
topology after a successful apply only.

**Disable needs the same precision.** The four rules above cover a *move*; a
plain disable (`mode = None` on an attached CRTC) has its own post-state, and
leaving it implicit is how half of it gets skipped. On success:

1. remove the output from the CRTC's `attached_outputs`;
2. zero the CRTC's `mode_id`, `x`, `y`, `width`, `height`;
3. set the output's current `crtc_id` to **0**.

Rule 3 is the one with no analogue in the move case and the easiest to miss —
without it the output still claims a binding to a CRTC that no longer lists it,
and `GetOutputInfo` publishes the inconsistency.

Tests: a deterministic A→B move asserting all four move postconditions including
A's zeroed geometry; a failed apply asserting A still owns the output and B is
still idle; and a disable of an attached CRTC asserting all three postconditions
above, in particular the output's `crtc_id == 0`.

**Cloning stays out of scope.** More than one output per CRTC is a real Xorg
capability our scanout pools — indexed parallel to `platform.outputs` — cannot
express. Reject with `BadMatch`, which is what Xorg does when the driver cannot
clone. The measurement shows this costs nothing on this hardware: each encoder's
`possible clones` is its own single bit, so amdgpu and i915 refuse cross-encoder
cloning anyway. Reject it *because we checked*, not as a side effect of a 1:1
assumption.

#### P3 extends `last_enabled` — remember the CRTC, not just the mode

**P1's remembered state stops being sufficient the moment P3 lands** (codex,
round 8). `last_enabled` records mode and position only, because under the 1:1
model the CRTC was not a choice. Under P3 it is:

> A client moves an output from CRTC A to CRTC B. The monitor is then physically
> disconnected and reconnected. P1's relight drives ordinary discovery, which
> picks the current-or-first route — **A** — and the output silently comes back
> on a different CRTC than the client selected, with no client request involved.

That violates P1's own stated contract ("restore the previous route policy") and
is a silent re-routing, which is worse than a visible failure.

**`ConnectorConfig::Enabled` gains the assigned CRTC as a stable RANDR XID**, not
a kernel handle. The XID is stable across disconnect/reconnect by construction —
that is what the allocator is for — whereas a `crtc::Handle` is only stable while
its device is present, which is precisely the case a reconnect may violate. It
also reuses the round-6 live-validated reverse lookup rather than adding a second
resolution path.

The relight step then selects **a currently usable tuple for that same CRTC**,
via P3c's route set. If no usable tuple for the remembered CRTC exists — the
CRTC is gone with its device, or is now occupied, or has no free primary plane —
leave the output Off and clear `last_enabled`, exactly as the incompatible-mode
case does. Never silently substitute a different CRTC.

Since `ConnectorConfig` is also the live `config` field, this addition is wanted
there anyway under P3: the live configuration should record which CRTC an output
is on, not just its mode and position.

Test: **reconnect after an A→B move.** Client moves the output to B, physical
disconnect, reconnect — assert it returns on **B**, not on A and not on
discovery's first pick. Plus the negative: B no longer usable at reconnect ⇒
output stays Off and `last_enabled` is cleared.

#### P3e — migration order

`RandrOutput::crtc_id` changes meaning from *identity* to *binding*. That is a
writers + readers + field flip, so per `feedback_atomic_switch_audits` the
intermediate states need auditing, not just the endpoints. Proposed order, each
step leaving the tree correct:

1. **P3a-1, additive.** Add `RandrState::crtcs` and populate it alongside the
   existing derivation. Nothing reads it yet. `screen_resources_current` still
   derives. No behaviour change.
2. **P3a-2, readers.** Switch `screen_resources_current` and `crtc_info` to the
   collection. Still 1:1, so output is byte-identical — this is the step where
   P2's attached-vs-possible split becomes expressible for an unpaired CRTC.
3. **P3a-3, allocation.** Move CRTC XIDs to `(DrmDeviceKey, crtc::Handle)`.
   **This is the step that changes advertised ids**, from one-per-connector to
   one-per-kernel-CRTC. Everything keyed on the old id must move together;
   enumerate those sites here rather than assuming the ~14 `crtc_id` mentions in
   `backend.rs` are all bindings — several are Present/pageflip CRTC handles in
   a different namespace.
4. **P3b, threading.** Requested CRTC end to end. Still 1:1-shaped because
   `possible_crtc_ids` is still a singleton, so the only observable change is
   that the CRTC we bind is provably the one asked for.

   **This commit also adds and populates `ConnectorConfig::Enabled.crtc_id` and
   snapshots it into `last_enabled`** (codex, round 9 — implied by "P3 extends
   `last_enabled`" but previously unnamed here). It belongs in P3b, not P3c: by
   step 4 the bound CRTC is already provably the requested one, so the value
   being recorded is meaningful, and it must be recorded *before* P3c lets a
   client pick a non-default CRTC — otherwise the first A→B move lands with
   nothing remembering B.
5. **P3c, widen.** Publish the real union. **This is the step that makes the
   xfce request succeed**, and the first step with a user-visible behaviour
   change.

**Two distinct kinds of visibility** — an earlier draft conflated them and
contradicted itself (codex, round 4):

| step | protocol-visible? | new routing capability? |
|---|---|---|
| P3a-1, P3a-2 | no | no |
| **P3a-3** | **yes — CRTC XIDs change**, one-per-connector → one-per-kernel-CRTC | no |
| P3b | no | no — binds provably what was asked, same set as before |
| **P3c** | yes | **yes — first step that accepts configurations we previously rejected** |

P3a-3 is an **identity migration**: clients see a different set of CRTC
resources, and a client caching ids across it will need the `config_timestamp`
bump, but nothing newly succeeds that failed before. P3c is the **capability**
change and the payoff — it is where xfce's `SetCrtcConfig` starts working.

That separation is the point of the ordering: a bisect lands either on "ids
moved" or on "new configs accepted", never on a half-flipped field.

## Invariants

1. A connector retired by a physical disconnect and returning with a compatible
   mode relights without any client request.
2. An idle CRTC reports zero attached outputs.
3. Every CRTC an output advertises in `crtcs[]` is one `SetCrtcConfig` will
   accept for that output.
4. RANDR output and CRTC XIDs stay stable across disconnect/reconnect —
   already true, must not regress.
5. No output ever lists a CRTC belonging to a different DRM device.
6. A client's explicit disable is never undone by an auto-relight.
7. A relit output never overlaps another output, and no surviving output moves
   because a restorable route departed.

## Test plan

Unit (deterministic, CI):

- `validate_set_crtc_config` accepts any CRTC in the output's possible set and
  `BadMatch`es one outside it, with `errorValue` matching Xorg's choice of field.
- `crtc_info` on an idle CRTC returns empty `outputs`, non-empty
  `possible_outputs`, and zeroed geometry.
- Drop → re-add of a connector key restores `Enabled{…}`; drop → re-add after a
  client disable does not.
- Re-add with a mode list that no longer contains the remembered mode leaves the
  output Off **and clears `last_enabled`**, so the next compaction reclaims the
  slot. Assert with a *second* rescan that the slot is not re-reserved — the
  single-rescan assertion passes even with the bug codex caught.
- **codex's overlap case, as a regression test**: A at x=0 + B at x=1920, drop
  A, assert B stays at x=1920 and the extent stays 5120 wide; re-add A, assert A
  is back at x=0 and the two do not overlap.
- Dropping an output with no `last_enabled` (never enabled) still compacts
  survivors — the reservation must not freeze the layout unconditionally.
- Cross-device: an output never lists a foreign CRTC.
- **P3b anti-silent-substitution**: after a `SetCrtcConfig` naming CRTC *C*
  succeeds, `GetCrtcInfo(C)` reports the output attached and every other CRTC
  reports it absent. This is the test that catches "advertise C, bind D".
- P3a: a kernel CRTC with no output appears in `GetScreenResources` with an
  empty attached list and a non-empty possible list.
- P3c: an output whose reachable CRTC set spans two encoders advertises the
  union, and binding a CRTC reachable only via the non-current encoder works.

Hardware (the gate — `feedback_no_commit_before_smoke`):

- **The reported repro**: awake XFCE, power-cycle monitor 2, no VT switch, no
  DPMS. Monitor returns with no client action, zero `BadMatch` on minor 21.
- The original DPMS repro from discussion #56, end to end.
- VT-away power-cycle — the path jos measured working; must stay working.
- `xrandr --output HDMI-3 --crtc <some other crtc> --auto` succeeds after P3.
- Dual-head extend/disable/re-enable via the XFCE display panel.

## Open questions for review

*Resolved in review: whether to gate P1's relight on `client_configured` — no,
restore every physically lost enabled route (codex; it is the reporter's own
configuration). Compaction ordering — resolved into the reserved-slot policy
under P1, after codex showed the current call site corrupts the layout. Which
sites assume `crtc_id` is an identity — folded into P3a/P3b as an enumeration
task rather than an open question. Async CRTC contention — answered by the
`RRSetConfigFailed` status byte, see P3b. **P1a vs P1b** — settled as P1b: it is
the proposed and now-approved design, and P3's CRTC-remembering extension builds
on `last_enabled`, so reopening P1a would invalidate that too. P1a's rationale
for rejection stays recorded under P1.*

1. Does P3a's `RandrCrtc` need to model leases (`RROutputIsLeased`, skipped in
   `rrcrtc.c:1224`)? We have no lease support; if that stays true, say so
   explicitly so the possible-output count is not silently wrong later.
2. `RANDR::SetCrtcTransform` is a stub xfce calls immediately before
   `SetCrtcConfig` (trace `#959`). It returns success so it is not the blocker,
   but it is on this exact path — in scope here or with the other unimplemented
   minors?

## Do not

- Do not call defects 2 and 3 PRIME regressions. They pre-date `a6f8909c` by
  months; #95 only exposed them.
- Do not "fix" this by making the rescan skip the teardown while DPMS is off.
  The teardown is correct; the missing relight is the bug, and the same wedge
  reproduces with no DPMS involved at all.
- Do not assume the VT-resume path is broken. It was measured working and the
  `vt_state` guard at `backend.rs:11998` explains why.
- Do not add a runtime env knob to select the old or new behaviour
  (`feedback_no_feature_kill_switches`). A/B via branch.
- Do not plan P3 as a plumbing change. The first draft did, and it was rejected:
  there is no CRTC object to widen and no path to carry the client's choice to
  KMS. P3a and P3b are prerequisites, not implementation detail.
- Do not reorder P3e. P3a-3 is a protocol-visible *identity* migration and P3c
  is the *capability* change; keeping them in separate commits is what lets a
  bisect distinguish "ids moved" from "new configs accepted".
- Do not relight a returning output onto whatever CRTC discovery picks. Once P3
  lands, the remembered CRTC is part of the route policy P1 promises to restore;
  substituting another one is a silent re-routing with no client request behind
  it. No usable tuple for the remembered CRTC ⇒ stay Off and clear
  `last_enabled`.
- Do not skip encoder re-selection on a union property. The union covering every
  CRTC does not mean the *current* encoder does; the retain test is per requested
  route. On the RX 6800 it is each individual encoder — not just the union — that
  reaches all six CRTCs, and that is what makes the skip valid there.
- Do not derive `possible_crtc_ids` from encoder masks alone. The primary-plane
  constraint is checked at preparation time, so an advertisement that ignores it
  breaks invariant 3 rather than failing early. On the RX 6800 the encoder masks
  are uniform `0x3f` while each primary plane drives exactly one CRTC — the two
  are not interchangeable.
- Do not invert the allocator map without intersecting the live topology. Ids
  are retained for stability, so a bare inversion can return a handle on a GPU
  that is gone.
- Do not resolve a `SetCrtcConfig` target by searching for an output already
  bound to the addressed CRTC. That is the 1:1 assumption in its most damaging
  form: it rejects every enable of an idle CRTC, which is the case P3 exists to
  make work. Enable resolves from `outputs[]`; disable resolves from
  `attached_outputs` and tolerates none.
- Do not put `DrmDeviceKey`, a `crtc::Handle`, or any other DRM type in a
  `yserver-core` struct. `yserver-core` depends only on `yserver-protocol`, and
  `DrmDeviceKey` is `pub(crate)` in `yserver` anyway. Device grouping that core
  genuinely needs goes through the existing RANDR 1.4 provider list.
- Do not ship P3a without P3b. Advertising a CRTC we then silently substitute is
  worse than today's honest `BadMatch` — it breaks invariant 3 while reporting
  success.
- Do not gate P1's relight on `client_configured`. Boot auto-layout sessions
  never set it and are exactly the reported configuration.
- Do not leave `recompact_horizontal_layout` where it is. Compacting inside
  `apply_connector_snapshot` runs before the relight decision and produces
  overlapping outputs on replug — P1 is not correct without the reserved-slot
  policy.
