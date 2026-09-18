# Xorg/MATE delegates hotplug relight to the desktop

> **Status: historical, conclusion superseded.** This trace establishes how
> MATE restores an output on Xorg. It does not establish that yserver may omit
> recovery: Awesome does not perform the corresponding RANDR configuration, and
> yserver then leaves a physically power-cycled output dark indefinitely.
> Restored P1 recovery was hardware-validated with Awesome (both output
> power-cycles and DPMS) and XFCE (ordinary power-cycle). The retained route is
> recovery state, not a claimed current RANDR configuration.

Design: [`2026-09-17-randr-crtc-model-and-hotplug-relight-design.md`](../specs/2026-09-17-randr-crtc-model-and-hotplug-relight-design.md)
(reviewed by codex over 9 rounds).
Plan: [`2026-09-17-randr-crtc-model-and-hotplug-relight-plan.md`](../plans/2026-09-17-randr-crtc-model-and-hotplug-relight-plan.md)
(4 rounds).
Commits: `96b0e292` relight + reservations, `eb064ee6` reservation-aware derived
extent, `5d1e29ab` root-storage growth.

Everything below is measured. Inferences are labelled as such.

---

## 1. What MATE actually does on stock Xorg

From `mate-xorg.xtrace`, real Xorg + MATE, dual 2560×1440, monitor 2
power-cycled. RANDR major opcode is **140** on this server (dynamic — do not
grep for 128). Sequence, with replies:

**On disconnect:**
```
SetCrtcConfig crtc=0x4e x=0 y=0 mode=0x59 outputs=0x56   → Success   (re-assert survivor)
SetCrtcConfig crtc=0x4f x=0 y=0 mode=0    outputs=       → Success   (DISABLE departed CRTC)
SetScreenSize 2560×1440                                             (shrink — ACCEPTED)
SetCrtcConfig crtc=0x4e x=0 y=0 mode=0x59 outputs=0x56   → Success
```

**On reconnect:**
```
SetCrtcConfig crtc=0x4e x=0 y=0 mode=0x59 outputs=0x56   → Success
SetScreenSize 5120×1440                                             (grow back FIRST)
SetCrtcConfig crtc=0x4f x=2560 y=0 mode=0x59 outputs=0x57 → Success  (client re-enables it)
SetCrtcConfig crtc=0x4e x=0 y=0 mode=0x59 outputs=0x56   → Success
```

Also measured in the same trace: 42 `GetOutputInfo` replies of the form
`crtc=0x4f connection=Disconnected(0x01)` — Xorg **does** retain the CRTC on a
disconnected output.

**Three things this establishes:**

1. **The client performs the entire restore on Xorg.** Grow the screen, then
   re-enable the CRTC. The server does nothing on its own.
2. **Xorg accepts the shrink**, because MATE disables the departed CRTC first.
   `rrscreen.c:266-279` iterates CRTCs *with a mode*, so after the disable
   nothing crops. The crop mechanism is real; the rejection never happens.
3. **MATE names the output's own former CRTC** (`0x4f` for output `0x57`), not a
   foreign one.

## 2. What happened on yserver with P1

From `yserver-hw-mate.log` (same box, same test, yserver 1.5.1 `96b0e292`):

```
06:59:36  render rescan: output HDMI-3 disconnected — dropping active scanout
06:59:36  render set_logical_screen_size: resized virtual screen to 2560×1440   ← MATE
06:59:59  render enable_connector: HDMI-3 enabled 2560×1440@60 at (2560,0); fb now 5120×1440
06:59:59  kms: relit HDMI-3 2560x1440@60 at (2560,0) after reconnect
```

Exactly one `set_logical_screen_size` in the whole log — the shrink. Zero RANDR
errors. **MATE issued neither the grow-back `SetScreenSize` nor the re-enabling
`SetCrtcConfig`.** Our auto-relight ran first, and MATE's apply found the CRTC
already correct.

*Inference, not measured:* MATE's diff-and-apply skipped its whole apply path,
including the `SetScreenSize`, because the CRTC half already matched. What is
measured is only that it sent neither request.

Visible result after `5d1e29ab`: the monitor returns and the background paints,
but the root window stays 2560 wide on a 5120 fb.

## 2b. The CRTC association survives physical loss but not an explicit disable

Raised by codex; settled from the same trace, so **no new experiment is needed
for this one**.

`GetOutputInfo` replies carrying `crtc=0x0000004f connection=Disconnected(0x01)`
occupy lines **38711–39493**. MATE's explicit
`SetCrtcConfig crtc=0x4f mode=0 outputs=` is at line **39545**. There are **no**
such replies after it.

| event | Xorg `GetOutputInfo.crtc` | yserver |
|---|---|---|
| physical disconnect | **retains `0x4f`** | `0` |
| explicit client disable | cleared to `0` | `0` |

yserver reports `0` for both, because `output_info` derives it from
`let assigned = out.mode_id != 0` (`crates/yserver-core/src/randr.rs:645`) and
reports `crtc: if assigned { out.crtc_id } else { 0 }`. A physically departed
output and a deliberately disabled one are indistinguishable to a client.

This is the likeliest reason MATE's restore state machine takes a different path
on yserver, and it is a protocol-surface difference, not a policy one.

**Note on where P1's one good idea belongs.** P1 already drew the physical-loss
vs client-disable distinction — that is exactly what `last_enabled` encodes. The
error was applying it to what the server *does* (relight) instead of to what the
server *reports*. The distinction is sound; its location was wrong.

## 3. What this refutes in the design

| design element | status |
|---|---|
| Server must auto-relight a returning connector | **Premise contradicted.** On Xorg the client does it, unprompted and successfully. |
| Reserved slots keep the layout stable | **Not Xorg behaviour.** MATE disables the CRTC and shrinks the screen; Xorg lets it. |
| Reservations should be visible as a retained CRTC (proposed P3 addition) | **Dead.** MATE does not rely on Xorg's retained CRTC — it tears down and rebuilds explicitly. |
| `screen_size_would_crop` should consider reservations | **Dead.** Xorg's equivalent check never fires, because the client disables first. |
| The A→B overlap hazard that drove 2 review rounds | **Self-inflicted.** It exists only because *we* auto-compact; Xorg has no auto-layout. |
| P3's `RandrOutput::crtc_id` = "current binding, 0 when unbound" | **Refuted** (codex). Xorg retains the association across physical loss — see §2b. Approved over 9 rounds and still wrong. |

## 4. What still holds, all independently measured

- `a6f8909c` (#95) deleted `PlatformBackend::requery_outputs_and_modeset`; the
  hotplug rescan no longer re-enables anything and `rescan.added_keys` is
  consumed by a `log::info!` alone. Git archaeology.
- `crtc_info` (`randr.rs:721`) reports its 1:1-paired output as both attached and
  possible even when disconnected with `mode_id == 0`. Xorg reports the attached
  set (`rrcrtc.c:1221`), empty for an idle CRTC. This is what made xfsettingsd
  carry a **disconnected** DP-2 into its request.
- The 1:1 model rejected a request legal on the hardware: xfsettingsd asked for
  `crtc=0x12 mode=0x13 outputs=0x11,0x05`, we returned `BadMatch bad=0x12`.
  `modetest` on silence shows every encoder reaching every CRTC (`0x3f` amdgpu,
  `0x0f` i915), and six primary planes each driving exactly one CRTC.
- Root backing storage was never grown on a hotplug extent change — fixed in
  `5d1e29ab`, hardware-confirmed (background now paints on the relit head).
- Window evacuation on disconnect is **identical on both servers** — MATE policy,
  not a divergence. Confirmed by jos on Xorg.

## 5. The untested A/B that decides the shape of the fix

**Replug under MATE on plain master (no P1).**

- MATE recovers unaided ⇒ P1 is the wrong shape. The reported bug is
  xfsettingsd-specific (it names a *foreign* CRTC, which the 1:1 model refuses),
  P3 is the real fix, and an unconditional server relight actively desynchronises
  well-behaved clients.
- MATE does not recover ⇒ P1 is needed, and the open work is making it not
  suppress the client's own screen-size restore.

Note this would vindicate jos's first instinct on discussion #56 — that the
failure was desktop-specific.

## 6. Direction given by codex, 2026-09-18

1. **Do not merge the P1 series** — `96b0e292`, `eb064ee6`, `5d1e29ab`. Pre-empting
   MATE's reconnect transaction makes it suppress both its `SetCrtcConfig` and
   the grow back to 5120. A 5120 KMS framebuffer under a 2560-wide root is a
   protocol-state desynchronisation, not a cosmetic follow-up.
2. KMS teardown on physical loss is **correct**; delete server-side relight and
   reservations.
3. Rework P2/P3 around **preserving the output's prior CRTC association across a
   physical disconnect**, while the CRTC itself may be idle with no attached
   outputs. P2's "idle CRTC has no attached outputs" stands and does *not* imply
   `GetOutputInfo.crtc` must go to zero.
4. Keep explicit client disable distinct from physical loss. *(Answered in §2b
   from the existing trace — Xorg clears the association on an explicit disable
   and retains it on physical loss.)*
5. Run the plain-master MATE replug A/B before writing the replacement spec, to
   see whether the association alone restores MATE or whether another mismatch
   remains.

**Net: P2 survives. P3's `crtc_id = 0 when unbound` premise does not.**

## 6b. Original questions, for the record

1. Should the server relight at all, or only as a **fallback** when no client
   restores the route within some window? A fallback needs a timeout, which the
   repo's "no kill-switches / no hedging" rule makes uncomfortable.
2. If relight stays: how should it avoid pre-empting a client that would have
   done it? Options not yet evaluated — publish the reconnect *before* relighting
   and relight only if nothing arrives; or relight but leave the screen size
   alone so the client's own `SetScreenSize` still fires.
3. Should reserved slots be deleted outright? They exist to protect an
   auto-layout that Xorg does not have, and they are why the screen-size chain
   started.
4. Does P2 (`crtc_info` attached-vs-possible) stand alone as a fidelity fix
   independent of all the above? It looks like the one piece that is
   unambiguously correct and unambiguously ours.
5. Is the right move to **trim** the design to P2+P3, or to rewrite P1 around
   "make the client's own restore succeed" rather than "do it for the client"?

## 7. Process note

Every claim in this document that came from a log, a diff or Xorg source has
survived contact with hardware. Every claim derived from the design's internal
logic has been refuted. Thirteen review rounds checked internal consistency and
could not reach the premise, because the premise was shared by author and
reviewer. One hardware run refuted it. See
`feedback_review_rounds_cannot_catch_scope_errors` in the agent memory.

## 8. P2 hardware result — the narrow fix closes the XFCE failure

P2 was implemented after this finding: idle `GetCrtcInfo` replies now carry an
empty attached-output array and keep the output only in the possible-output
array. A fresh XFCE run on 2026-09-18 then enabled the second display.

The trace shows the causal change directly. Before P2, XFCE reused the idle
CRTC's falsely attached output and sent `crtc=0x12 outputs=0x11,0x05`, which
failed with `BadMatch`. After P2, `xfce.xtrace:22650` reports for CRTC `0x06`
`outputs=[] possible=[0x05]`; XFCE sends `SetCrtcConfig crtc=0x06
outputs=0x05` at `xfce.xtrace:133870`, and yserver replies `Success` at
`xfce.xtrace:134021`.

Therefore P1 is rejected and P3 is deferred. The report does not justify a
larger CRTC-model rewrite: the client-visible defect was the conflation of
attached and possible outputs.
