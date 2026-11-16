# Pointer routing aligned with Xorg — design

Status: draft, 2026-11-16. Base: joske/master d8d3416b.

## Intent

The maintainer chose this order on #370: first align pointer event routing with
Xorg, in its own PR and without access control; then revise the access-control
4c plan on top of the corrected master. This PR is that first step. It fixes
routing defects that exist today with no policy installed, so that the later
Receive checks are added to Xorg-shaped delivery instead of writing today's
divergences into the access-control spec.

Reference: Xorg is cited as `../xserver` at tag xorg-server-21.1.24.

## Goals

Routing contract for goals 1-3 (mirrors `DeliverDeviceEvents`,
`../xserver/dix/events.c:2895-2930`, and grab delivery, `:4354-4462`):

- **Per device form.** The slave event and its master copy are delivered in
  separate passes (`../xserver/mi/mieq.c:484-508`); each pass uses that
  device's own grab (`../xserver/Xi/exevents.c:1901,1942-1951`). An ungrabbed
  form keeps normal delivery: with an XIGrabDevice on master 2 and no slave
  grab, a client selecting slave-4 XI_Motion still receives the slave form.
  The owner-only rule never applies a master grab to the slave form or the
  reverse.
- **Per window, in Xorg's order.** On each window of the natural walk try XI2,
  then XI1, then core. Within one attempt the grab owner is delivered to and
  every other selector is rejected (`:2279-2313`, `TryClientEvents` -1 for a
  non-owner, `:2071-2075`). A rejected XI2 attempt does not end the window:
  XI1 and core are still tried there. A positive delivery ends the walk.
  Propagation stops at the window when its final result is non-zero
  (`:2924-2927`), so a window where only foreign clients selected blocks the
  walk.
- **Fallback.** When the natural walk delivers nothing, the event goes to the
  grab window if the grab's mask selects it (`:4431-4462`).

1. **Owner-events grab, XI2 natural delivery.** Today
   `pointer_fanout.rs:1894-1897` keeps every natural XI2 selector in
   `xi2_targets` under an owner-events grab: with A grabbing master 2 with
   owner_events and B selecting master `XI_Motion`, `XI_ButtonPress` and
   `XI_ButtonRelease` on the window under the pointer, B receives those
   events; under Xorg B receives none of the master form. Apply the contract.
2. **The same rule on the smooth-scroll Motion path** (`pointer_fanout.rs`
   ~2097-2113), which builds its own natural target list.
3. **Core owner-events walk.** `grabbed_natural_target` already stops at a
   foreign core selection and falls back to the grab window. It must also
   treat a window where another client holds a matching master-form XI2
   selection as a rejected attempt, then still try core on that window
   (A's own core selection there is delivered). A slave-only XI2 selection
   does not block the core walk (`fanout.rs:147-159`).
4. **XI1 `MORE_EVENTS`.** `encode_xi1_device_input_event` writes
   `deviceid & 0x7f` (`xinput/mod.rs` ~1392) and clears the bit the caller set
   when a DeviceValuator follows; Xorg keeps it (`dix/eventconvert.c:369-370`).
   Keep the bit.

Every expected result in goals 1 and 3 comes from a measured Xorg run (see
Evidence). Goal 2 is source-backed (see Evidence: scroll). No expected result
comes from reasoning alone.

## Must keep (regression cases these paths came from)

- **Core implicit-grab exception** (`pointer_fanout.rs` ~1887, the XFCE dialog
  fix): a core implicit grab does not take XI2 delivery away from XI2
  selectors. It stays unless a measured Xorg run shows otherwise; the goal-1
  rule applies to explicit grabs and XI2 implicit grabs only, unless that
  measurement says it covers this case too.
- **Slave then master walk** (`mi/mieq.c:502-508`): with slave-level XI2
  selections two implicit grabs activate and one client may legitimately get
  both deliveries (Enlightenment 0.27). The existing E27 tests stay green.
- No release may be lost: every press delivered to a client gets its release.

## Non-goals

- Access control, Receive checks, the 4c plan revision (follows this PR).
- **A second master device.** `XI2_ABSORBING_POINTER_DEVICES` hardcodes
  master id 2 (`fanout.rs:159`), but yserver cannot create a master:
  `XIChangeHierarchy` AddMaster answers BadAlloc
  (`process_request/xi/mod.rs` ~372). The gap is unreachable; the PR text
  says so and nothing changes there.
- Rewriting delivery as one `DeliverDeviceEvents`-style walk. Considered and
  rejected (see Approaches).
- Keyboard, XKB, device notifications, Enter/Leave routing.
- No new `YSERVER_*` environment variables.

## Approaches

1. **Targeted fixes (chosen by the user).** Change the owner-events filter on
   the XI2 and scroll paths, add the XI2 stop to the core grabbed walk, keep
   the XI1 bit. Each change carries its measured Xorg case. Small, reviewable;
   what the maintainer suggested ("may be smaller than a single walk").
2. Single Xorg-style walk: closer in structure, but a large diff in an area
   that has lost button releases before, with more hardware risk.
3. As 1 with `MORE_EVENTS` in its own earlier PR: one more PR for two lines;
   the maintainer accepts either, so it stays in this PR.

## Evidence

- **Oracle:** Xvfb built from the local xorg-server-21.1.24 tree
  (`/data/Projects/xorg-21.1.24`, meson, no root, no GPU), routing lives in
  dix/Xi/mi and is the same code Xorg runs. Small test clients (any
  non-copyleft language/library; scratch in
  `~/Projects/claude-scratch/pointer-routing/`) set up each case with XTest
  input and record what every client receives. The same clients run against
  yserver headless; the results must match.
- **Cases to measure, at least:** owner-events XI2 grab with a foreign
  `XI_Motion` / button selector on (a) the grab window, (b) a descendant,
  (c) a window owned by the grab client outside the grab window; the same for
  a core GrabPointer with owner_events and a foreign XI2 selector below; the
  core implicit grab (XFCE case); slave + master XI2 selections (E27 case);
  an XI1 motion with valuators beyond the first event (`MORE_EVENTS`). Each
  case names the grabbed device, the selected device and the event types.
- **Scroll is source-backed.** Xvfb/XTest has no scroll-capable source (the
  XTest pointer and the vfb mouse have only X/Y axes,
  `../xserver/Xext/xtest.c:620-622`, `../xserver/hw/vfb/InitInput.c:81-107`;
  smooth-scroll Motion needs a scroll axis, `../xserver/dix/getevents.c:1646-1692`).
  Goal 2's expected results are derived from the same contract as goal 1 with
  the cited lines; its yserver test injects through a registered
  `PhysicalPointer` with a scroll class, the production scroll input path
  (`pointer_source_has_scroll_class`). Pinned scroll expectations change only
  with such a citation. The hardware smoke covers real wheel scrolling.
- **Existing tests** (except scroll, above) that pin today's owner-events behaviour change their
  expected values only where the Xorg measurement gives a different answer;
  the PR lists each changed expectation with its measured case.
- **Hardware smoke** (user's machine, his permission, he watches): Steam
  (Chromium/CEF), Cinnamon menus, XFCE dialogs, Enlightenment 0.27, including
  synchronous grabs and replay.

## Test rules

- Tests drive the real request and input paths (real grab requests, real
  selections, real injected input), not hand-built internal state.
- Each fix has a test that fails on today's master and a named mutation that
  makes it fail again.
- Load runs: the pointer/grab tests 5 times under 16 busy loops.

## Provenance

Xorg (MIT) is the behavioural reference and may be quoted. No copyleft code is
read or used.

## Left to the builder

Measurement client implementation, test file placement, exact helper names,
and how the owner-only filter reuses `grabbed_natural_target` or the existing
target computation (reuse first; new code only where needed).
