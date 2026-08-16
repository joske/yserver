# PRIME bb4 semantic replay decisions (2026-08-17)

## Scope

This note records the adaptation of original commit `bb4ef852`
(`feat(prime): finish device-qualified topology and lifecycle`) to the current
KMS render backend. The replay keeps the original device-identity intent, but
does not mechanically restore policies that conflict with the current
direct-scanout, cursor, or lightweight-probe implementations.

The replayed slice covers:

- all-device connector inventory and lifecycle reconciliation;
- device-qualified Present clocks, vblank arms, and event routing;
- device-qualified RANDR gamma state; and
- a conservative boundary around upstream's grouped direct-scanout path.

Per-card hardware-cursor ownership is deliberately split into a subsequent
adaptation after per-Present device routing. The multi-device inventory added
before and in this slice supplies the device/output routing that follow-up
needs, but the current cursor manager still owns one primary-device cursor
resource.

## Connector probing and failure policy

Startup seeds the stable RANDR registry from a lightweight connector probe on
every opened DRM device. Forced RANDR queries use the same minimal
connection/mode probe. Physical hotplug and VT-resume instead gather an
enriched connector-only snapshot for live-survivor metadata, still without
performing CRTC/plane assignment; assignment is deferred until a client
actually enables a connector. Connector identity is
`(DRM device key, connector name)`, so two
cards may both expose `DP-1` without sharing output/CRTC XIDs or connection
state. Connected secondary-device outputs enter the registry off and remain
dark until a RANDR client enables them.

Forced RANDR queries remain strictly lightweight and reconcile only connection
state plus advertised modes. Physical hotplug/resume snapshots additionally
carry size, EDID, and connector type so an already-live `Output` can refresh
its connector-owned metadata without inventing a new route. Those values are
transient in this slice: connected-but-Off/disconnected registry entries do
not persist physical identity, and forced-query metadata authority remains for
the later `9beaf545` successor. Enabling one connector uses target-only
discovery with every same-card survivor's encoder, CRTC, and primary plane
reserved; it cannot steal a survivor's live objects merely because a
whole-card hypothetical allocator enumerated the target first.

For this replay, a failed probe is an error. All-device probing gathers
a complete result before mutating the combined RANDR snapshot, so failure on
one card is never translated into "every connector on that card disappeared"
and cannot leave a half-reconciled registry.

The current callers surface that error as follows:

- initial topology seeding fails backend construction;
- forced `RRGetScreenResources` refresh returns `BadAlloc`, following the
  existing core behavior;
- VT resume logs the probe error and requests server shutdown; and
- an asynchronous hotplug rescan logs the error and leaves the previously
  reconciled state untouched.

This deliberately simple policy assumes that GPUs do not normally disappear.
It does not add retry or stale-state machinery in this replay.

### Deferred secondary-probe policy

If real hardware shows transient secondary-card probe failures, a later change
may make only that case best effort:

1. warn and retain the last successfully reconciled state for an inconclusive
   secondary-device error;
2. retry on a bounded schedule; and
3. retire the device only after confirmed removal or persistent `ENODEV`.

That policy must distinguish an unavailable device from a disconnected
connector. It must not infer disconnection from one failed ioctl, preserve a
stale device forever, or partially publish a new multi-device snapshot. It is
explicitly deferred and is not current behavior.

## Topology mutation and buffer lifetime

Physical removal and RANDR reconfiguration must not drop a framebuffer,
scanout pool, or Vulkan allocation while an old CRTC or queued page-flip event
can still refer to it. This replay therefore gathers the complete snapshot
first, disables the old CRTC set before mutating output vectors, consumes the
old fd-qualified events, drains scene resources, and only then resets pools or
applies the new topology. A synchronous modeset reserves its selected pool BO
as the new on-screen front. Survivors are re-lit and have gamma restored; if
the server cannot prove all-off or cannot re-light the resulting topology, it
fails closed and requests shutdown rather than continuing with ambiguous
buffer ownership.

## Present ownership and clocks

Raw CRTC handles are scoped to one DRM fd and may collide across cards. The
replay therefore identifies a CRTC as `(DRM device key, CRTC handle)` for:

- general UST/MSC samples and completion-eligible clocks;
- software-MSC fallback counters;
- relative idle-vblank arms and absolute target arms;
- sequence-event routing and arm retirement; and
- lifecycle pruning after output removal.

A page-flip or sequence event updates only the CRTC belonging to the fd that
delivered it. Relative idle pacing may arm each live CRTC on its own device.
An absolute Present target is armed only on the device-qualified CRTC currently
supplying the server's maximum Present clock; it is not broadcast to every
device. The protocol-facing clock remains the existing maximum reduction over
live CRTC samples.

`DRM_IOCTL_CRTC_QUEUE_SEQUENCE` support is latched per DRM device. An
unsupported ioctl on one card leaves sequence arming available on other cards.

This maximum reduction is a compatibility bridge, not the final ownership
model. Protocol-side pending Presents still share one clock in this commit.
The immediately following adaptation carries a selected RANDR CRTC/device
through each Present, groups arms by that domain, and preserves per-window MSC
continuity when a window moves between unrelated device counters.

RANDR and XF86VidMode gamma requests now carry the RANDR CRTC XID through the
backend boundary. KMS resolves that XID to its device-qualified output key, and
gamma caches use the same key, so equal connector names on different cards do
not share a LUT or route an ioctl to the wrong fd.

## Direct-scanout scheduling boundary

Ordinary composition already submits and retires page flips per output. That
is the correct path for outputs with different refresh frequencies: each CRTC
continues at its own cadence.

Upstream's current direct path is different. One compositor-authoritative
whole-root Present supplies all output crops, and direct entry plus composed
replacement are grouped into one all-CRTC atomic transaction. Splitting that
transaction naively would allow partial direct/composed ownership, complicate
source-pin and Idle retirement, and reintroduce the mixed-state failures that
the grouped unflip was designed to avoid.

Consequently, grouped direct scanout is eligible only when every active output
is on the primary DRM device and every output has the same effective refresh
timing. Effective refresh compares exact pixel-clock/total ratios, including
interlace and doublescan adjustments, rather than only rounded integer Hz.
Mixed-device layouts and heterogeneous-refresh layouts stay on ordinary
per-output composition. A single active output on a non-primary device also
stays composed because the current direct-import resources belong to the
primary device.

A future per-output direct path could relax this boundary only after it defines
independent authoritative-source ownership, partial-success rollback, and
per-output completion/Idle lifetimes.

## Per-card hardware-cursor follow-up

The desired policy is best-effort hardware cursors independently on each DRM
card, with software composition only on the card/output whose cursor path is
unavailable. The repository now has stable device keys, a vector of opened KMS
devices, and output-to-device routing, but the cursor plane, pending move, and
unsupported latch are still singleton platform state. This replay therefore
does not claim that multiple hardware cursors are already managed correctly.

The subsequent per-card cursor adaptation should:

- move cursor-plane ownership, pending motion, and fallback state to the
  owning KMS device;
- restore and adapt bb4's distinct cursor-plane-to-CRTC coverage matching per
  device, while retaining the optimistic legacy-ioctl path when a driver does
  not expose universal-plane metadata;
- route upload, bind, move, hide, resume, and teardown operations by output
  device;
- preserve hardware cursors on unaffected cards when one card falls back to
  software; and
- add same-handle/different-device and independent-fallback regression tests.

The follow-up must preserve current runtime probing. In particular, `EINVAL`
is ambiguous: a disabled CRTC, a temporary atomic state, or a bad parameter can
produce it even when hardware cursors are supported. It must remain non-sticky.
Only errors that establish unsupported device behavior may latch that device
to software; transient or ambiguous failures are warned and retried through
the normal lifecycle.

Deferred cursor initialization after a genuinely headless start remains the
separate later `ed8fcad4` successor; this follow-up must not absorb it.
