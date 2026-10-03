# Dynamic XInput device registry for KMS input

**Status:** approved design, 2026-09-29.  **Branch:** `feat/xi-dynamic-registry`.

**Implementation:** authorized by the user on 2026-09-30. Further Opus reviews are canceled. Local reset/VT questions are settled by the concrete contracts in keyboard/pointer Tasks 3 and 18; task implementation/review proceeds with the previously chosen workers. The interrupted round-4 record remains historical, without a convergence verdict.

## Purpose and observed failure

yserver currently publishes masters 2/3 and one slave pointer/keyboard pair
4/5. On each libinput device add, a pointer with available acceleration can
replace the metadata and libinput property owner of device 4. On this machine,
both the Razer DeathAdder V3 `event4` and the HyperX Alloy Origins 65 Mouse
`event9` advertise a pointer with acceleration. The latter can therefore own
device 4 even while motion comes from the Razer. The i3 command
`xinput set-prop 4 "libinput Accel Profile Enabled" 0 1 0` then configures the
wrong libinput source. `xinput list` exposes only one physical pointer facade.

The objective is for every usable libinput keyboard and pointer function,
including laptop touchpads, to have its own XInput identity and for all
clients to see and configure the same live devices. No exact device selector or heuristic for a
"primary mouse" is part of the design. Acceleration is configured per
device through its own XI properties; there is no server-wide default
(see "Mouse acceleration configuration").

## Scope revision: direct touch is out of scope

**Revised 2026-10-02 (user decision):** the intended scope is mice,
keyboards and laptop touchpads. Direct-touch devices (touchscreens, touch
tablets) are an explicit **non-goal**; their support entered this design
and its adversarial review without being requested, and the companion
touch plan is dropped. A touchpad remains a pointer facet classified `TOUCHPAD` with its
own libinput properties, and touchpad gestures never imply an XI2
TouchClass. A touch-only libinput source gets no XI facet, and libinput
touch events are not translated, exactly as before this design. Wherever
this document still mentions `Touch` facets, XI2 TouchClass, touch
contacts, `TOUCHSCREEN` or touch delivery, that text is historical and not
a requirement.

## Grounding and compatibility

- Linux can expose several `/dev/input/event*` nodes for one USB HID device.
  A libinput device has independent keyboard, pointer, and touch capabilities;
  a libinput device group correlates nodes but does not select a preferred
  pointer. Razer and HyperX each expose genuine mouse-capable event nodes.
- [wlroots' libinput backend](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/backend/libinput/events.c)
  creates an input object for each announced capability and retains their
  common source. It does not select one physical pointer for the cursor.
- Xorg has a master pointer/keyboard pair and an XTEST slave pair; physical
  devices are separate slaves. Its [core device initialization](https://gitlab.freedesktop.org/xorg/xserver/-/blob/master/dix/devices.c)
  creates the XTEST pair after the masters. yserver will reserve 2/3 for
  masters and 4/5 for virtual XTEST slaves, then allocate physical facets
  starting at 6. The IDs 4/5 remain present, but they will not alias a
  physical device or expose its libinput properties.
- The existing [touchpad XI2 property design](2026-06-02-touchpad-xi2-properties-design.md)
  intentionally assumes one physical slave pointer. This design supersedes
  that topology while retaining its property encoding, XI1 touchpad atom,
  device node, and XI1/XI2 enumeration consistency requirements.
- [MATE's mouse manager](https://github.com/mate-desktop/mate-settings-daemon/blob/master/plugins/mouse/msd-mouse-manager.c)
  calls `XListInputDevices`, probes each device's libinput properties, and
  uses XI1 `DevicePresenceNotify` to reapply settings after hotplug. XI2-only
  registration is insufficient.

This scope is the direct KMS/libinput backend. Nested host input retains
master/core delivery and produces master XI device/raw forms with
`deviceid=sourceid=2` for pointer and `3` for keyboard, without a physical
slave form. XTEST retains its virtual or explicitly requested XI device
identity; nested input is never attributed to XTEST.

## Model and ownership

The input backend assigns a monotonic `SourceId` to each new physical
endpoint attachment. `SourceId` is internal and lives through that
attachment's removal. Libinput handle identity binds ordinary events to
the source; evdev node is descriptive metadata, not the runtime identity,
and `eventN` can change or be reused after reconnection. An explicit VT
suspend/resume pair may rebind a new libinput handle to the existing source
only after proving the same kernel endpoint instance, as described below.
Every ordinary physical remove/add creates a new source. The libinput group
is optional physical correlation metadata, never a key for XI routing or a reason to merge event
nodes. Vendor/product and name are metadata, not device selectors.

The central registry owns:

- `SourceId -> SourceRecord`: live capability set, group metadata if
  available, evdev node, name, vendor/product, enabled/suspended state,
  current confirmed libinput configuration snapshot, and the XI facet IDs
  created from that source. A bounded VT continuation key is metadata used
  only while pairing one explicit suspend with its resume.
- `XiId -> XiDevice`: role, attached master, source/facet reference for
  physical slaves, classes, enabled state, and independent property map.
- `SourceId + facet -> XiId`: event attribution and property routing.

One libinput source may create one keyboard facet and one pointer/touch
facet. The facets have different XI IDs even when they share the same
`SourceId`. `Keyboard` creates a slave keyboard attached to master 3.
`Pointer` creates a slave pointer attached to master 2. `Touch` creates a
slave pointer with XI2 TouchClass attached to master 2; if the same source
also has `Pointer`, its touch and pointer classes share that XI pointer
facet. A touchpad is a pointer classified as `TOUCHPAD`; ordinary libinput
touchpad gestures do not imply an XI2 TouchClass. A source with keyboard
and pointer capabilities, such as Razer `event5` or HyperX `event11`, gets
both facets even if it rarely emits pointer motion. Pointer capability does
not imply available acceleration.

IDs 0/1 keep their XI wildcard meaning, 2/3 are masters, and 4/5 are
virtual XTEST slaves. Physical IDs are allocated from 6 through 127. XI1
`xDeviceInfo.id` is eight bits, but the high bit of XI1 event `deviceid` is
reserved for `MORE_EVENTS`; this range keeps every advertised slave usable
by XI1 event clients as well as XI2 clients. A freed ID may be reused only
after the old source and all its facets are removed and the removal has
been published. The internal `SourceId` prevents a queued event or backend
configuration request from reaching a later device that reuses an XI ID or
an `eventN`. If IDs are exhausted, log and keep that source in the input
inventory for core/master delivery only; omit device-specific XI events
for its unpublished facet. Do not overwrite a live facet or report a
nonexistent slave.

## Device lifecycle and events

At `DeviceAdded`, the libinput backend captures all three capability bits,
the source ID, input metadata, and the full supported configuration
snapshot, without applying any server-wide acceleration default. It stores a libinput handle by `SourceId` for
writable properties, including sources with several facets. Registration
creates all facets for one source before publishing a hierarchy change.

Every translated keyboard, pointer, scroll, and touch event carries its
`SourceId` into the core loop. The registry selects the corresponding
facet; core keyboard/pointer delivery continues through masters 3/2.
XI2 device and raw events use the actual physical facet as their source,
with Xorg-compatible `deviceid`/`sourceid` fields and selection behavior.
XTEST defaults to its virtual slaves 4/5; a valid explicit XI device target
retains that target, as in Xorg. Nested host input keeps master/core delivery
without being labeled as XTEST. Unknown or already removed source IDs are dropped;
known sources without an allocated facet retain core/master delivery where
core delivery exists, with master-only XI device/raw forms using
`deviceid=sourceid=2` or `3`, but no physical slave form. Unknown, removed,
or suspended physical input is rejected before cursor, XKB, held state,
recording or fanout can change. It must never be attributed to another
device. Touch begin/update/end preserve contact
IDs for the lifetime of each contact. Removing a source
releases held keys/buttons and active contacts through the existing
master/focus paths before the facets disappear.

Held state belongs to each XI slave, including XTEST and explicit XTEST
targets; unpublished physical sources retain equivalent internal state.
Slave duplicate guards are separate from master guards. Master button
release is suppressed while another attached slave holds that mapped
button. Master key press is suppressed if the key is already down, but the
first valid attached-slave release releases the master key even if another
slave still holds it, matching Xorg. Floating slaves do not modify master
state and floating keyboards have separate XKB state. Master XKB/core
modifiers change only on accepted master transitions; a slave press whose
master press is suppressed cannot increment the master's XKB down count.
KMS integrates
physical relative motion from accelerated fractional deltas into the
current master or floating-slave position; an input-thread cursor must not
feed stale absolute positions back after touch, warp or floating motion.

One guarded cleanup owner drains held keys/buttons and touch contacts
before disabling a source. Synchronous VT release invokes it before
yielding the session; later suspend messages are idempotent. Pause/resume
commands are delivered in FIFO order, including when both arrive within
one input-thread drain. Drain physical devices only, including injected
holds targeting those physical facets; enabled masters and virtual XTEST
4/5 retain their holds. Preserve master locked Caps/Num state and restore
current locks/LEDs on resumed keyboards without a fresh XKB-state reset.
There are no origin-less synthetic modifier releases on resume. Clear the
master's last-slave reference on disable as well as removal. Physical removal then
removes all facets/properties atomically and publishes Removed after
Disabled, without allocating their IDs again before publication.

VT suspend preserves source/XI identities, properties, selections and
current settings, marks inventory facts disabled, and retires libinput
handle bindings. Resume drains initial enumeration and, on Linux, proves
continuation by the captured canonical `/sys/class/input/<sysname>` target
including the kernel `inputN/eventM` instance, never by name, vendor/product,
group or a reused evdev node. This internal proof is bounded to that one VT
pair; it adds no user selector. Keep unmatched provable sources disabled
through a fixed 2500 ms resume retry window and match late opens during
that window, with the existing 250 ms dispatch retry even after an empty
initial enumeration. Only deadline expiry removes still-unmatched sources;
unrelated hotplug cannot extend this continuation deadline. Unprovable
keys follow safe logged remove/add. A proven continuation first runs normal
touchpad setup, then restores saved recognized settings on the new handle
before gathering actual values and enabling the same facets. Failed
restorations are logged and queries report actual gathered values. Preserve
ordinary properties and deleted-property absence on continuation; refresh
only present driver properties. Lifecycle precedes that source's buffered
input. New writes to suspended sources fail BadMatch in Xorg's validation
order, without waiting for the global lane. Only backend commands already
submitted before pause may resolve after rebind or as SourceGone at failed
continuation. Ended active grabs and contacts are not resurrected.

Server reset replays enabled and suspended sources from the process-lifetime
inventory with their attachment identity and current confirmed config.
Before destroying the old clients/facets, a backend input-session reset
hook retires old held state, repeats, queued input and touch ownership.
Fresh XI IDs never inherit those maps. New-session XKB/LED state is
initialized from startup defaults; VT lock preservation does not apply
to a server-generation reset. Live source identity/configuration and
in-progress process-level recovery remain intact. A new VT pause preserves
unmatched recovery facts and invalidates the old timer; expiry is checked
against the current monotonic window token, and failed resume starts no
window. These contracts have explicit checks in the implementation plan.

Submitted configuration completions remain process-lifetime and update
live source facts even if their original client or server generation has
ended; old atom/sequence metadata cannot leak into the new generation.

Publish XI2 `XI_HierarchyChanged` on physical facet add/remove and
`XI_DeviceChanged` when an existing device's classes change. Publish XI1
`DevicePresenceNotify` at `first_event + 15`, with its special
`0x10000 | _devicePresence` event class and Xorg's Added then Enabled,
Disabled then Removed transitions, so MATE reapplies settings after hotplug. XI2
publishes the corresponding hierarchy steps with the full live-device list
and removed descriptors where required. Disabled precedes registry deletion;
Removed follows it. Before an attached master's source changes, publish
`XI_DeviceChanged` with reason SlaveSwitch and that source's classes; scroll
values belong to each source. XI1
`SelectExtensionEvent` must retain this selection rather than silently
discard it. Property changes continue to emit XI1
`DevicePropertyNotify` and XI2 `XI_PropertyEvent` for the affected facet.

## Query and property protocol

`XIQueryDevice` and XI1 `XListInputDevices` iterate one registry snapshot.
They return the same live ID set, names, roles, and compatible class/type
information. The XI1 descriptor retains registry attachment metadata, but
Xorg's `Xi/listdev.c::ListDeviceInfo` leaves the legacy `xDeviceInfo.attached`
wire byte zero; XI2 reports the live attachments. XI1 reports
`MOUSE`, `KEYBOARD`, `TOUCHPAD`, or
`TOUCHSCREEN` as appropriate; type atoms are interned at server start.
XI2 emits the classes supported by each facet, including TouchClass only
where libinput reports touch. Queries for one ID and XI wildcard IDs obey
the same registry. A hotplugged device is visible in both APIs before its
presence/hierarchy notification is delivered. The XI1 encoder must accept
an arbitrary list; its current four-entry layout cannot remain.
`XOpenDevice` opens listed slave devices and returns classes for the selected
facet. Masters remain listed but return BadDevice, matching
`Xi/opendev.c::ProcXOpenDevice`; unknown IDs return BadDevice as well.

`XIListProperties`, `XIGetProperty`, XI1 property requests,
`xinput list-props`, `XIChangeProperty`, and `XIDeleteProperty` resolve the requested
XI ID to its own property map. Every physical facet, including keyboard,
exposes read-only `Device Node` and `Device Product ID`. Pointer/touch
facets expose only configuration supported by that source's capability
snapshot. Seeded driver properties reject direct deletion; defaults,
availability and metadata descriptors reject writes. GetProperty(delete)
follows Xorg's separate reply-and-unlink path without checking deletable;
XI2 Deleted notification requires nonzero returned length, while XI1 sends
it whenever delete && bytes_after == 0. Recognized write support and
read-only rules derive from source facts/descriptor identity, independent
of whether a map entry exists. A supported writable property deleted by
GetProperty is recreated after backend success with Created notification
and Xorg's default deletable=true; existing entries retain their flags.
Ordinary application properties remain writable/deletable. Keyboard facets
do not inherit pointer acceleration properties merely because the source
also has pointer capability.
Writes to recognized libinput properties validate, apply to that source's
live handle, and commit the XI value only after backend success, preserving
the existing Tier 2b rule. Queueing a KMS input-thread command is not backend
success: its completion must carry the libinput result back to the core.
Confirmed changes also update the atom-free input inventory; a client
disconnect or server reset cannot erase an already applied backend change.
XI device IDs, rather than a global pointer
slot, select the target. A queued recognized write captures its source at
receipt, validates and merges when it reaches the runner's serialized
configuration lane, and commits through the same inventory update for
synchronous and asynchronous success. It cannot follow a reused XI ID to
another source. Removed-source queued requests report BadDevice using the
original request sequence. Virtual devices 4/5 have no seeded physical
metadata or driver configuration. Masters/XTEST may store ordinary
application properties with libinput names, as Xorg permits, but these
never invoke a physical backend. The `XTEST Device` marker on 4/5 is
read-only by atom identity and protected against direct deletion; its
GetProperty(delete) path follows the same Xorg behavior.

Audit yserver code paths that hardcode 4/5 as physical slave IDs: XI1
device validity/open/grabs and event classes; XI2 selections, hierarchy,
raw/device events and grabs; property access, reset inventory, source
removal, and device-changed fanout. This is an internal code audit, not a
survey of third-party X11 clients. A physical event must never be stamped
as source 4 solely because 4 is the only current slave. Preserve core
event behavior via masters 2/3.

## Mouse acceleration configuration

**Revised 2026-10-02 (user decision):** the earlier `YSERVER_MOUSE_ACCEL_PROFILE`
startup default is dropped. The upstream maintainer asked on PR 129 not to
add gating environment variables, since they become a permanent
maintenance burden. No server-wide acceleration default is added.

Each acceleration-capable pointer facet exposes its own libinput
properties, so clients configure physical devices directly. MATE's mouse
manager reapplies its settings to each device on XI1 `DevicePresenceNotify`
after hotplug. Without a settings daemon, a startup script can configure
every device that exposes the property, which is the same pattern the
project's vng pointer scenarios use:

```sh
for id in $(xinput list --id-only); do
    xinput set-prop "$id" 'libinput Accel Profile Enabled' 0 1 0 2>/dev/null || true
done
```

Sources without acceleration support do not expose the property, so the loop skips
them. The existing i3 `set-prop 4` line must be replaced, because 4 is now
the virtual XTEST pointer and never configures a physical mouse. A device
plugged in after startup keeps libinput's default until a client
configures it.

## Touch protocol details (dropped)

The touch protocol details recorded by the 2026-09-30 adversarial review
(TouchClass axes, contact delivery, touch grabs, pointer emulation) belonged
to the dropped direct-touch scope; see "Scope revision" above and the
findings documents for the historical text.

## Review correction record

The 2026-09-30 corrections and their task mapping are recorded in
[the first adversarial findings](../findings/2026-09-30-dynamic-xinput-adversarial-plan-review.md)
and [round 2](../findings/2026-09-30-dynamic-xinput-adversarial-review-round-2.md)
and [round 3](../findings/2026-09-30-dynamic-xinput-adversarial-review-round-3.md)
with their correction dispositions.
They preserve the approved registry topology, virtual 4/5 and absence of an
exact selector. The keyboard/pointer plan contains 18 separate task boundaries;
the touch plan follows with 10. Implementation began 2026-09-30; progress is
tracked in `docs/status.md`.

## Acceptance criteria for the implementation plan

- With the provided Razer/HyperX inventory, `xinput list` shows distinct
  pointer IDs for Razer `event4` and HyperX `event9`, keyboard facets for
  keyboard-capable nodes, and both facets for mixed nodes. The exact
  physical IDs may change across restarts; 2/3/4/5 retain their roles.
- XI1 and XI2 enumerate identical live IDs and names before and after
  add/remove. MATE receives XI1 device-presence notifications and can
  configure every pointer that exposes acceleration.
- A property write to the Razer pointer changes only the Razer libinput
  handle. The same write to the HyperX pointer changes only HyperX. A
  write to one mixed source's keyboard facet cannot alter its pointer
  facet; writes to 4/5 cannot alter physical devices.
- Without client configuration, libinput's default acceleration remains
  in force on each source; a client write to one source's acceleration
  property changes only that source (no server-wide default exists).
- Pointer and keyboard events carry their actual source IDs; master/core
  behavior, focus, grabs, scroll, XTEST, and existing touchpad properties
  continue to work. (Direct-touch contacts are out of scope.)
- VT switching releases held keys/buttons/contacts exactly once, preserves
  IDs, selection masks and confirmed settings for proven continuations,
  and exposes Disabled/Enabled without false Removed/Added transitions.
- Capacity exhaustion, unsupported properties, duplicate/stale events,
  and rapid unplug/replug do not rebind an old XI facet to another source.

The implementation plan should split registry/lifecycle, XI1 and XI2
enumeration, source-aware event routing, and hotplug notices into
reviewable stages. Each stage must preserve
the invariants above; the feature is complete only when all stages are
integrated. No implementation or tests are run by this design document.
