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

Per-card hardware-cursor ownership is split into the subsequent incremental
adaptation after per-Present device routing. The multi-device inventory added
before and in this slice supplies its device/output routing; the adaptation
now gives every already-active startup card its own cursor resource and
fallback state.

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

That maximum reduction was the compatibility bridge at the bb4 commit
boundary, not the final ownership model. The subsequent adaptation now carries
a selected RANDR CRTC XID and physical-route epoch through each Present,
groups every arm by that domain, and preserves per-window MSC continuity when
a window moves between unrelated device counters. Explicit valid-but-Off CRTCs
remain accepted but unpaced, and zero-output implicit Presents use synthetic
domain 0. Because a stable RANDR CRTC XID may later resolve to another raw KMS
CRTC, an epoch change fails old queued work open instead of comparing or arming
it against the replacement counter.

Implicit Pixmap requests reselect by greatest window/output intersection, with
the active RANDR primary winning equal-area ties; NotifyMSC reuses the window's
last selected domain. The request snapshots its Xorg-style MSC offset, so a
later window move cannot reinterpret an older target or completion. Grouped
whole-root direct scanout still withholds completion until every participating
CRTC retires, but records the selected/reference CRTC's exact sample for the
wire event.

Present destination windows also carry a non-reusable lifetime generation.
Window or final overlay destruction purges every core-visible request and
clock binding; a completion already hidden in a render batch or direct frame
is discarded when its old generation eventually retires, so reusing the XID
cannot receive stale Complete or Idle events. If direct scanout owns that
window or the COW fallback, destruction, unmapping, geometry changes, and
logical-screen resizing first materialize the authoritative frame and request
a composed replacement. Source and COW pins remain live until the grouped
replacement retires.

The core deliberately retains the last samples for superseded physical-route
epochs. A copy or direct completion can remain hidden inside the backend after
the corresponding core-visible queue has disappeared, so pruning from core
state alone could discard the only clock needed to stamp that event. This is
one small cache entry per route epoch; compacting it safely is deferred until
the backend exposes epoch-reference retirement.

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

## Per-card hardware-cursor adaptation

Hardware cursors are now best effort independently on each DRM card, with
software composition only on the card/output whose cursor path is unavailable.
Each `KmsDevice` owns its cursor plane, latest-wins EBUSY retry, permanent
unsupported latch, topology-coverage decision, and device-qualified per-CRTC
transient state. Every bind/move/hide resolves the output's device key before a
raw CRTC handle can reach an fd, so numerically equal CRTC handles on two cards
cannot cross-route.

The adaptation:

- moves cursor-plane ownership, pending motion, and fallback state to the
  owning KMS device;
- restores and adapts bb4's distinct cursor-plane-to-CRTC coverage matching per
  device, while retaining the optimistic legacy-ioctl path when a driver does
  not expose universal-plane metadata;
- routes upload, bind, move, hide, resume, and teardown operations by output
  device;
- preserves hardware cursors on unaffected cards when one card falls back to
  software;
- evaluates NVIDIA's software-cursor policy and driver cursor dimensions per
  owning card, so card order does not leak policy and a 96px cursor may remain
  hardware on a 128px plane while a 64px card composes it; and
- adds same-handle/different-device, distinct-plane matching, device-local
  fallback, and transaction-outcome regression tests.

`EINVAL` remains non-sticky because a disabled CRTC, temporary atomic state, or
bad parameter can produce it even when hardware cursors are supported. A
failed operation first rolls back any live bind before software composition is
allowed. Once safely unbound, only that output composes in software for a
bounded number of its own successful frame retirements; repeated `EINVAL`
probes use an exponential 1/2/4/8-frame cap. A topology/resume boundary or a
sprite geometry/hotspot change clears the transient observation. Pixel-only
animation versions do not reset the backoff. `ENXIO`, `ENODEV`, and
`EOPNOTSUPP` latch only the owning device; EBUSY remains a device-local
latest-position retry drained only when that device retires a flip.
An active-startup transient plane-construction failure is recorded separately
and retries only at an explicit active-topology or resume boundary. A device
that had no startup CRTC stays in a distinct deferred state. The standalone
`ed8fcad4` successor consumes that state only after a successful explicit
RANDR enable has inserted the first `ActiveOutput`: it passes every
post-insertion active CRTC on the owning card to the factory before scene/RANDR
rebuild. Ordinary probes, connected-Off rescans, failed enable paths, frames,
and zero-card startup do not allocate. A transient lazy failure retries only at
a later topology/resume boundary; a permanent failure latches only that card.
Once allocated, the plane persists across last-output disable/re-enable.

Cursor show is transactional across the legacy bind and move ioctls. If move
fails, a hide rollback must succeed before the scene records Hidden/SW; if the
prior or new hardware binding remains visible, the scene retains actual HW
mode and last-known metadata, forces a full Show retry, and never draws a
second software cursor over it. Hw→Sw/Hidden is a separate two-phase handoff:
the first retiring frame omits the software cursor, then attempts hide. Hide
failure keeps the old HW sprite as the only cursor and submits another
cursorless hide-retry frame; success permits the SW cursor only on the next
frame, accepting a one-frame cursor gap to preserve the no-double invariant.
The pending reveal remains Mixed/non-direct until that SW frame retires.
Sprite and hotspot Show retries are version-qualified so an older or unrelated
frame retirement cannot clear newer work. Any cursor fallback or pending
rebind unwinds active direct scanout before the composed retry.

The lazy first-output behavior above is kept in the separate `ed8fcad4`
successor rather than folded into the per-card manager commit.
