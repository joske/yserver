use std::{
    collections::{HashMap, HashSet},
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    },
    time::Instant,
};

use log::trace;
use yserver_protocol::x11::{
    self, AtomId, ClientByteOrder, ClientId, ResourceId, SequenceNumber, shape, xfixes,
};

use crate::{
    randr::{RandrOutput, RandrState},
    resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_WINDOW, ResourceTable},
};

pub const FIRST_CLIENT_BASE: u32 = 0x0010_0000;
pub const PER_CLIENT_MASK: u32 = 0x000F_FFFF;

/// First event code reserved for the XInput extension (XI1 + XI2 share
/// one contiguous block; the constant has an `XI2_` prefix in
/// [`crate::nested`] for historical reasons). Re-exported here under an
/// XI-version-neutral name so the XI1 `SelectExtensionEvent` plumbing in
/// `process_request.rs` can derive `XEventClass` low bytes
/// (`first_event + event_code`) without naming the misleading `XI2_*`
/// symbol.
///
/// `DevicePropertyNotify` (XI1 event code 16) therefore lives at
/// `XI_FIRST_EVENT + XI_DEVICE_PROPERTY_NOTIFY_OFFSET = 82`.
pub(crate) const XI_FIRST_EVENT: u8 = crate::nested::XI2_FIRST_EVENT;

#[derive(Debug)]
pub struct IdAllocator {
    next_base: u32,
    /// Bases of fully-disconnected clients whose resources were torn
    /// down (`close_mode = DestroyAll`). Reused before bumping
    /// `next_base` so XID-heavy workloads (xts5 XIproto opens a fresh
    /// connection per protocol-validation TP and burned through bases
    /// after ~4096 reconnects, hitting the u32 ceiling) keep going.
    /// `Vec` rather than `VecDeque` because allocate/release strict
    /// LIFO order is fine — Xorg's `clientPrivates` reuses MRU bases
    /// similarly. Released bases of *retained* clients (CloseDownMode
    /// `RetainPermanent`/`RetainTemporary`) are NOT pushed here — the
    /// resources owned by those bases stay live in the resource table
    /// and a fresh client reusing the base would collide with them.
    free_bases: Vec<u32>,
}

impl IdAllocator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_base: FIRST_CLIENT_BASE,
            free_bases: Vec::new(),
        }
    }

    /// Returns `(resource_id_base, resource_id_mask)` for a new client.
    /// Reuses a previously-released base if one is available; otherwise
    /// bumps the monotonic counter. Returns `None` only when the free
    /// list is empty AND the next monotonic base would overflow `u32`
    /// — in practice unreachable for any realistic workload now that
    /// disconnect recycles bases.
    pub fn allocate(&mut self) -> Option<(u32, u32)> {
        if let Some(base) = self.free_bases.pop() {
            return Some((base, PER_CLIENT_MASK));
        }
        let base = self.next_base;
        let next = base.checked_add(FIRST_CLIENT_BASE)?;
        self.next_base = next;
        Some((base, PER_CLIENT_MASK))
    }

    /// Return a previously-allocated base to the free list for reuse
    /// by future `allocate` calls. The caller is responsible for
    /// ensuring all resources owned by `base` are destroyed first —
    /// see [`crate::core_loop::process_disconnect`] for the
    /// retain-aware caller.
    pub fn release(&mut self, base: u32) {
        // Sanity: only recycle bases we actually handed out.
        // `(base & !PER_CLIENT_MASK) == base` (mask-aligned) and
        // `base >= FIRST_CLIENT_BASE` (above the server's own range).
        if (base & PER_CLIENT_MASK) != 0 || base < FIRST_CLIENT_BASE {
            log::warn!("IdAllocator::release: ignoring invalid base 0x{base:x}");
            return;
        }
        self.free_bases.push(base);
    }

    /// `id` is owned by the holder of `(base, mask)` iff `(id & !mask) == base`.
    #[must_use]
    pub fn validate_owned(id: u32, base: u32, mask: u32) -> bool {
        (id & !mask) == base
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct AtomTable {
    by_name: HashMap<String, AtomId>,
    names: HashMap<u32, String>,
    next_id: u32,
}

impl AtomTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            names: HashMap::new(),
            next_id: 69, // X11 predefined atoms are 1..=68; custom atoms start here
        }
    }

    pub fn intern(&mut self, name: &str, only_if_exists: bool) -> AtomId {
        if let Some(atom) = x11::well_known_atom(name) {
            return atom;
        }
        if let Some(atom) = self.by_name.get(name).copied() {
            return atom;
        }
        if only_if_exists {
            return AtomId(0);
        }
        let atom = AtomId(self.next_id);
        self.next_id += 1;
        self.by_name.insert(name.to_owned(), atom);
        self.names.insert(atom.0, name.to_owned());
        atom
    }

    #[must_use]
    pub fn name(&self, atom: AtomId) -> Option<&str> {
        x11::well_known_atom_name(atom).or_else(|| self.names.get(&atom.0).map(String::as_str))
    }

    #[must_use]
    pub fn exists(&self, atom: AtomId) -> bool {
        atom.0 != 0
            && (x11::well_known_atom_name(atom).is_some() || self.names.contains_key(&atom.0))
    }

    /// Register a synthetic name-atom pair at a caller-chosen id. Used
    /// only by tests that need a specific numeric atom (e.g. 100/200
    /// fixtures predating the T3 BadAtom guard); production code goes
    /// through [`Self::intern`] which assigns ids sequentially.
    #[cfg(test)]
    pub(crate) fn register_for_test(&mut self, atom: AtomId, name: &str) {
        self.by_name.insert(name.to_owned(), atom);
        self.names.insert(atom.0, name.to_owned());
        if atom.0 >= self.next_id {
            self.next_id = atom.0 + 1;
        }
    }
}

#[derive(Debug, Clone)]
pub struct PassiveButtonGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    /// 0 = AnyButton
    pub button: u8,
    /// 0x8000 = AnyModifier
    pub modifiers: u16,
    /// X11 `GrabButton` / XI2 passive-grab `owner_events` flag.
    /// When true, events on windows owned by the grab client should
    /// still be delivered normally instead of being redirected to
    /// `grab_window`.
    pub owner_events: bool,
    pub event_mask: u32,
    pub pointer_mode: u8,
    /// 0 = GrabModeSync: the grab activation also freezes the
    /// KEYBOARD on the grab's behalf (Xorg CheckGrabForSyncs).
    pub keyboard_mode: u8,
    /// Window the pointer is confined to while the grab is active
    /// (0 = none) — X11 GrabButton `confine_to`.
    pub confine_to: ResourceId,
    /// True when the grab was established through the XI2 protocol
    /// (XIPassiveGrabDevice) rather than core GrabButton. Grab
    /// redirection delivers to the owner in the protocol the grab
    /// was established with (Xorg `DeliverGrabbedEvent` consults the
    /// grab's own xi2mask, which is empty for core grabs) — sending
    /// XI2 XGE events to a core-only Xlib client NULL-derefs libXi's
    /// wire handler when the client linked libXi without ever doing
    /// XIQueryVersion (xts5 Xlib11/ButtonPress TP10 crashed there).
    pub via_xi2: bool,
}

#[derive(Debug, Clone)]
pub struct KeyGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    /// 0 == AnyKey
    pub keycode: u8,
    /// 0x8000 == AnyModifier; otherwise the literal modifier-state mask
    pub modifiers: u16,
    pub owner_events: bool,
    /// 0 = Synchronous, 1 = Asynchronous
    pub pointer_mode: u8,
    /// 0 = Synchronous, 1 = Asynchronous
    pub keyboard_mode: u8,
    /// True when established via XI2 (XIPassiveGrabDevice keycode
    /// grab) rather than core GrabKey — see
    /// [`PassiveButtonGrab::via_xi2`] for the delivery-protocol rule.
    pub via_xi2: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ActiveKeyboardGrabSource {
    /// from GrabKeyboard
    Explicit,
    /// activated by a passive GrabKey on the matching keycode press
    PassiveKey { keycode: u8 },
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveKeyboardGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub source: ActiveKeyboardGrabSource,
    /// X11 `owner_events`: when true, key events that would naturally
    /// be reported to one of the grab client's windows are reported
    /// normally instead of being redirected to `grab_window` (Xorg
    /// `DeliverGrabbedEvent`).
    pub owner_events: bool,
    /// True when established via XI2 (XIGrabDevice on the master
    /// keyboard, or an activated XI2 passive key grab) — see
    /// [`PassiveButtonGrab::via_xi2`] for the delivery-protocol rule.
    pub via_xi2: bool,
}

/// XI 1.x passive device grab (GrabDeviceKey / GrabDeviceButton).
#[derive(Debug, Clone, Copy)]
pub struct Xi1PassiveGrab {
    pub owner: ClientId,
    pub deviceid: u16,
    pub grab_window: ResourceId,
    /// keycode or button; 0 == AnyKey / AnyButton
    pub detail: u8,
    /// 0x8000 == AnyModifier; otherwise the literal modifier mask
    pub modifiers: u16,
    pub owner_events: bool,
    /// 0 = Synchronous, 1 = Asynchronous (this device)
    pub this_mode: u8,
    /// 0 = Synchronous, 1 = Asynchronous (other devices)
    pub other_mode: u8,
    /// true: key grab; false: button grab
    pub is_key: bool,
}

/// XI 1.x active device grab (GrabDevice, or an activated passive
/// device grab).
#[derive(Debug, Clone, Copy)]
pub struct Xi1ActiveGrab {
    pub owner: ClientId,
    pub deviceid: u16,
    pub grab_window: ResourceId,
    pub owner_events: bool,
    pub this_mode: u8,
    pub other_mode: u8,
    /// Some(detail) when activated from a passive key/button grab —
    /// auto-released when that detail is released.
    pub passive_detail: Option<u8>,
}

/// Core keyboard focus state — Xorg `FocusClassRec` (win/revert/time)
/// for the virtual core keyboard.
#[derive(Debug, Clone, Copy)]
pub struct CoreFocus {
    /// Raw wire focus value: `0` = None, `1` = PointerRoot, anything
    /// else a window xid.
    pub raw: u32,
    /// 0 = RevertToNone, 1 = RevertToPointerRoot, 2 = RevertToParent.
    pub revert_to: u8,
    /// Last focus-change time — SetInputFocus requests with an earlier
    /// time are ignored (Xorg `focus->time`).
    pub time: u32,
}

impl Default for CoreFocus {
    /// Xorg initial focus (dix/devices.c `InitFocusClassDeviceStruct`):
    /// PointerRoot with RevertToNone.
    fn default() -> Self {
        Self {
            raw: 1,
            revert_to: 0,
            time: 0,
        }
    }
}

/// XI 1.x per-device freeze bookkeeping for synchronous grabs.
#[derive(Debug, Default)]
pub struct Xi1Freeze {
    /// Per-device sync state — Xorg `GrabInfoRec.sync.state`
    /// (include/inputstr.h:504-511). `FrozenNoEvent`/`FrozenWithEvent`
    /// freeze the device outright; `FreezeNextEvent`/`FreezeBothNextEvent`
    /// arm a re-freeze on the next key/button event delivered through
    /// the grab (`FreezeThisEventIfNeededForSyncGrab`,
    /// dix/events.c:4420-4447).
    pub state: Xi1SyncState,
    /// Frozen on behalf of ANOTHER device's sync grab — Xorg
    /// `sync.other != NullGrab`. Stores that grab's owning client; in
    /// the two-device model the source grab is always the paired
    /// device's (or its bridged core grab), so owner identity is exact.
    pub other: Option<ClientId>,
    /// The event stored when `state == FrozenWithEvent` — the replay
    /// source for AllowDeviceEvents(ReplayThisDevice) (Xorg
    /// `sync.event`).
    pub stored: Option<Xi1QueuedEvent>,
    /// Device events queued while frozen, replayed on thaw (Xorg
    /// `syncEvents.pending`, kept per-device here).
    pub queue: std::collections::VecDeque<Xi1QueuedEvent>,
    /// CORE key events withheld while the keyboard device is frozen —
    /// in Xorg the freeze switches the whole device to the enqueue
    /// proc, so core delivery stops too. Only the keyboard device uses
    /// this (core POINTER events deliberately keep flowing for now —
    /// desktop interactivity risk outweighs XTS fidelity there).
    pub core_key_queue: std::collections::VecDeque<crate::host_x11::HostKeyEvent>,
}

impl Xi1Freeze {
    /// Xorg `ComputeFreezes` predicate: a device is frozen when it is
    /// directly frozen OR held on behalf of another device's grab.
    #[must_use]
    pub fn frozen(&self) -> bool {
        self.other.is_some() || self.state >= Xi1SyncState::FrozenNoEvent
    }
}

/// Xorg `GrabInfoRec.sync.state` values (include/inputstr.h:504-511).
/// `NOT_GRABBED`/`THAWED` collapse to [`Xi1SyncState::Thawed`] — both
/// mean "not frozen, nothing armed". Ordering is load-bearing:
/// `>= FrozenNoEvent` is Xorg's `>= FROZEN` check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Xi1SyncState {
    #[default]
    Thawed,
    /// AllowDeviceEvents(SyncThisDevice): thawed until the next
    /// key/button event delivered via the grab, then FrozenWithEvent.
    FreezeNextEvent,
    /// AllowDeviceEvents(SyncAll): like FreezeNextEvent, but the next
    /// delivered event also re-freezes the paired device.
    FreezeBothNextEvent,
    /// Sync grab established, no event stored (active GrabDevice).
    FrozenNoEvent,
    /// Frozen with the triggering event stored (passive sync grab
    /// activation, or FreezeNextEvent tripping) — Replay material.
    FrozenWithEvent,
}

/// XI 1.x per-device focus state (SetDeviceFocus / GetDeviceFocus).
///
/// `focus` is the raw wire value: `0` = None, `1` = PointerRoot, `3` =
/// FollowKeyboard (XI.h), anything else a window xid. Keyboard devices
/// default to PointerRoot / RevertToNone, matching Xorg
/// `InitFocusClassDeviceStruct` (dix/devices.c:1494-1495).
#[derive(Debug, Clone, Copy)]
pub struct Xi1DeviceFocus {
    pub focus: u32,
    /// 0 = RevertToNone, 1 = RevertToPointerRoot, 2 = RevertToParent,
    /// 3 = RevertToFollowKeyboard.
    pub revert_to: u8,
    /// Last-focus-change time (server ms).
    pub time: u32,
}

impl Default for Xi1DeviceFocus {
    fn default() -> Self {
        Self {
            focus: 1, // PointerRoot
            revert_to: 0,
            time: 0,
        }
    }
}

/// XI 1.x per-device input state reported by DeviceStateNotify — the
/// port of Xorg's `dev->key->down` / `dev->button->down` bitmasks and
/// the valuator mode set via SetDeviceMode. Maintained by
/// `xi1_route_device_event` (the single point every XI1 key/button
/// event flows through, frozen-queue replays included).
/// Default: nothing down, Relative mode (0) — matches the
/// ListInputDevices valuator class.
#[derive(Debug, Clone, Copy, Default)]
pub struct Xi1DeviceInputState {
    /// Key-down bitmask, one bit per keycode (keycode N → byte N/8,
    /// bit N%8) — Xorg `KeyClassRec.down`.
    pub keys_down: [u8; 32],
    /// Button-down bitmask, same layout — Xorg `ButtonClassRec.down`.
    pub buttons_down: [u8; 32],
    /// Valuator mode: Relative=0 / Absolute=1 (XI.h). Set by
    /// SetDeviceMode; reported in DeviceStateNotify `classes_reported`
    /// bits above `ModeBitsShift`.
    pub valuator_mode: u8,
    /// Current axis values — Xorg `ValuatorClassRec.axisVal`. Real
    /// motion writes the sprite position into axes 0/1; device-motion
    /// fakes write their explicit axis payload (device fakes do NOT
    /// move the sprite — verified against Xephyr). Reported by
    /// DeviceStateNotify / QueryDeviceState / motion deviceValuator
    /// chains.
    pub valuators: [i32; 4],
}

/// How the final SelectExtensionEvent step of `xi1_route_device_event`
/// resolves its target — the XI1 analogue of Xorg's
/// `DeliverFocusedEvent` (dix/events.c:4202). Grab/freeze handling is
/// unaffected by this; only the no-grab selection delivery is gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Xi1FocusRoute {
    /// Unbounded selection walk up from `natural_target` (pointer
    /// events; keyboard focus PointerRoot).
    Walk,
    /// Selection walk from `natural_target` bounded at (inclusive) the
    /// focus window — the focus is an ancestor of the pointer window,
    /// so propagation must stop at the focus boundary.
    WalkUpTo(ResourceId),
    /// Deliver to exactly this window's selectors, no propagation —
    /// the pointer is outside the focus subtree, so the event reports
    /// relative to the focus window itself.
    WindowOnly(ResourceId),
    /// Device focus is None: discard (no selection delivery).
    Drop,
}

/// A device input event in delivery-ready form — what the XI1 fanout
/// needs to route one event, and what freeze queues hold for replay.
#[derive(Debug, Clone, Copy)]
pub struct Xi1QueuedEvent {
    pub deviceid: u16,
    /// Absolute XI1 wire event code (XI_FIRST_EVENT + offset).
    pub evcode: u8,
    pub detail: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state_mask: u16,
    /// Natural (hit/focus) target the selection walk starts from.
    pub natural_target: ResourceId,
    /// Device-focus gating for the selection-walk step.
    pub focus_route: Xi1FocusRoute,
    /// Explicit axis payload for device-motion fakes
    /// (XTestFakeDeviceMotionEvent): carried into the deviceValuator
    /// chain and written to the device's stored axis values. `None`
    /// for real motion (axes 0/1 = sprite position).
    pub axes: Option<Xi1MotionAxes>,
    /// AllowDeviceEvents(ReplayThisDevice) reprocessing hint: passive
    /// grabs at or above this window are skipped, "as though they were
    /// not present" (Xorg replays via CheckDeviceGrabs starting one
    /// below `syncEvents.replayWin`). `None` for normal routing.
    pub replay_floor: Option<ResourceId>,
}

/// Axis payload of a faked device motion (first_valuator, count,
/// values) — Xorg keeps these in the event's valuator mask.
#[derive(Debug, Clone, Copy)]
pub struct Xi1MotionAxes {
    pub first: u8,
    pub count: u8,
    pub values: [i32; 6],
}

#[derive(Debug, Clone, Copy)]
pub struct ActivePointerGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub event_mask: u16,
    /// 0 = inherit
    pub cursor: ResourceId,
    pub time: u32,
    /// X11 `GrabPointer` / XI2 `XIGrabDevice` `owner_events` flag.
    /// When true, pointer events on windows owned by the grab client
    /// are delivered normally (to the deepest natural window) rather
    /// than redirected to `grab_window`. This is how GTK3 menus
    /// expect motion + click events to flow during a popup grab —
    /// the panel button stays "hover-tracked" until the pointer
    /// crosses into the popup itself, at which point natural
    /// `EnterNotify`/`LeaveNotify` fire and GTK3 transitions menu
    /// state. With `owner_events=false`, every event is reported
    /// against `grab_window` with no propagation.
    pub owner_events: bool,
    /// True when established via XI2 (XIGrabDevice) rather than core
    /// GrabPointer — see [`PassiveButtonGrab::via_xi2`] for the
    /// delivery-protocol rule.
    pub via_xi2: bool,
}

/// XComposite redirect mode. Both wire constants are accepted —
/// `Automatic` (update=0) and `Manual` (update=1) — but the
/// redirected-backing pixmap path is unimplemented, so no code
/// currently branches on the variant. The record's presence is what
/// `NameWindowPixmap` and the disconnect-cleanup paths consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeRedirectMode {
    Manual,
    Automatic,
}

/// Global DPMS extension state. Mirrors Xorg's per-server (not
/// per-screen) DPMS data model. `kms_capable` is snapshotted from
/// the backend at init and never changes; `enabled` mirrors Xorg's
/// `DPMSEnabled` and starts equal to `kms_capable` (Xorg
/// `Xext/dpms.c:587`).
#[derive(Debug, Clone)]
pub struct DpmsState {
    pub kms_capable: bool,
    pub enabled: bool,
    /// 0=On, 1=Standby, 2=Suspend, 3=Off.
    pub power_level: u8,
    /// 0 means "this level disabled" (Xorg `os/WaitFor.c:403-410`).
    pub standby_ms: u32,
    pub suspend_ms: u32,
    pub off_ms: u32,
    pub last_activity: Instant,
    /// Client IDs that issued `DPMSSelectInput(DPMS_INFO_NOTIFY_MASK)`.
    pub selected_by: HashSet<ClientId>,
}

impl DpmsState {
    /// Initial state — built lazily from the backend's
    /// `dpms_capable()` at `ServerState::new(...)` time. Defaults
    /// match Xorg: timeouts = `ScreenSaverTime` (600s) ×3.
    #[must_use]
    pub fn new(kms_capable: bool) -> Self {
        Self {
            kms_capable,
            enabled: kms_capable,
            power_level: 0, // On
            standby_ms: 600_000,
            suspend_ms: 600_000,
            off_ms: 600_000,
            last_activity: Instant::now(),
            selected_by: HashSet::new(),
        }
    }
}

/// Activation state of the screensaver. `Cycle` is used only as the
/// `notify_state` argument to `emit_screen_saver_notify` from the
/// periodic cycle path; it never appears in
/// `ScreenSaverState.active`, which only holds `Off` or `On`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ScreenSaverActive {
    Off,
    On,
    Cycle,
}

/// Global MIT-SCREEN-SAVER state. Mirrors Xorg's per-server (not
/// per-screen) saver data model. The idle clock lives on `DpmsState`
/// (`last_activity`) — both extensions read the same "time of last
/// input" baseline.
#[derive(Debug, Clone)]
pub struct ScreenSaverState {
    /// `SetScreenSaver` `timeout` field, in milliseconds. 0 = idle
    /// timer disabled.
    pub timeout_ms: u32,
    /// `SetScreenSaver` `interval` field. We don't implement Internal
    /// saver tiling, but `GetScreenSaver` echoes the stored value and
    /// `interval_ms` drives the `ScreenSaverNotify(state=Cycle)` re-fire
    /// while active.
    pub interval_ms: u32,
    /// Echo-only — `GetScreenSaver` round-trip. No behavioural effect.
    pub prefer_blanking: bool,
    /// Echo-only — `GetScreenSaver` round-trip.
    pub allow_exposures: bool,

    /// Current activation. Holds only `Off` / `On`; never `Cycle`.
    pub active: ScreenSaverActive,
    /// True when the most recent transition came from
    /// `ForceScreenSaver` or from DPMS→SS coupling. Mirrors the
    /// `forced` byte on `ScreenSaverNotify` wire events.
    pub forced: bool,

    /// Per-client `SelectInput` mask. OR of `SCREEN_SAVER_NOTIFY_MASK`
    /// (0x01) and `SCREEN_SAVER_CYCLE_MASK` (0x02). Xorg's
    /// `ProcScreenSaverSelectInput` (`saver.c:695-713`) does NOT
    /// validate bits — any value is stored verbatim; only the two
    /// mask bits gate delivery. `mask == 0` removes the entry.
    /// QueryInfo's `event_mask` reply field is the CALLING client's
    /// mask (`saver.c:220-231`), not the union.
    pub selected_by: HashMap<ClientId, u32>,

    /// Per-client outstanding `Suspend(true)` count. Effective
    /// "suspended" = `!suspend_counts.is_empty()`. `Suspend(false)`
    /// decrements saturating to 0 (matches Xorg's silent
    /// `FreeResource` on spurious free); on hitting 0 the entry is
    /// dropped. `process_disconnect` drops the entry entirely.
    pub suspend_counts: HashMap<ClientId, u32>,

    /// Instant the next `ScreenSaverNotify(state=Cycle)` should fire.
    /// Set to `Some(now + interval_ms)` whenever `active` transitions
    /// to `On` (when `interval_ms > 0`); advanced each cycle fire;
    /// cleared when `active` returns to `Off`. Mirrors Xorg
    /// `WaitFor.c:473-476`.
    pub next_cycle: Option<Instant>,
}

impl ScreenSaverState {
    /// Defaults match Xorg `dix/globals.c:96-99`:
    /// `defaultScreenSaverTime` = `defaultScreenSaverInterval` = 600s,
    /// `defaultScreenSaverBlanking = PreferBlanking`,
    /// `defaultScreenSaverAllowExposures = AllowExposures`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout_ms: 600_000,
            interval_ms: 600_000,
            prefer_blanking: true,
            allow_exposures: true,
            active: ScreenSaverActive::Off,
            forced: false,
            selected_by: HashMap::new(),
            suspend_counts: HashMap::new(),
            next_cycle: None,
        }
    }
}

impl Default for ScreenSaverState {
    fn default() -> Self {
        Self::new()
    }
}

/// Core keyboard control state (`ChangeKeyboardControl` /
/// `GetKeyboardControl` / `Bell`). Defaults mirror Xorg's
/// `defaultKeyboardControl` (`dix/globals.c:63` + `include/input.h`
/// `DEFAULT_*`).
#[derive(Debug, Clone)]
pub struct KeyboardControlState {
    /// 0..=100. `DEFAULT_KEYBOARD_CLICK` = 0.
    pub key_click_percent: u8,
    /// 0..=100. `DEFAULT_BELL` = 50.
    pub bell_percent: u8,
    /// Hz. `DEFAULT_BELL_PITCH` = 400.
    pub bell_pitch: u16,
    /// Milliseconds. `DEFAULT_BELL_DURATION` = 100.
    pub bell_duration: u16,
    /// Global auto-repeat. `DEFAULT_AUTOREPEAT` = TRUE.
    pub global_auto_repeat: bool,
    /// Per-key auto-repeat bitmap (keycode N → byte N>>3, bit N&7).
    pub auto_repeats: [u8; 32],
    /// LED bitmask (led N → bit N-1). `DEFAULT_LEDS` = all off.
    pub led_mask: u32,
}

/// Xorg `include/input.h` `DEFAULT_AUTOREPEATS`: keycodes 8..=135
/// repeat, the rest don't.
pub const DEFAULT_AUTO_REPEATS: [u8; 32] = [
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

impl KeyboardControlState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            key_click_percent: 0,
            bell_percent: 50,
            bell_pitch: 400,
            bell_duration: 100,
            global_auto_repeat: true,
            auto_repeats: DEFAULT_AUTO_REPEATS,
            led_mask: 0,
        }
    }
}

impl Default for KeyboardControlState {
    fn default() -> Self {
        Self::new()
    }
}

/// Core pointer control state (`ChangePointerControl` /
/// `GetPointerControl`). Defaults mirror Xorg's
/// `defaultPointerControl` (`dix/globals.c:74`): accel 2/1,
/// threshold 4.
#[derive(Debug, Clone)]
pub struct PointerControlState {
    pub accel_numerator: u16,
    pub accel_denominator: u16,
    pub threshold: u16,
}

impl PointerControlState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            accel_numerator: 2,
            accel_denominator: 1,
            threshold: 4,
        }
    }
}

impl Default for PointerControlState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-window XComposite redirect record stored in
/// [`ServerState::composite_redirects`]. The `owner` is the client
/// that issued the `RedirectWindow` / `RedirectSubwindows` — used
/// by the dispatch layer for `BadAccess` conflict detection and by
/// `process_disconnect` to tear down redirects belonging to a
/// departing client (L2 task B.1b).
#[derive(Debug, Clone, Copy)]
pub struct RedirectRecord {
    pub mode: CompositeRedirectMode,
    pub owner: ClientId,
}

#[derive(Debug)]
pub struct ServerState {
    pub atoms: AtomTable,
    pub resources: ResourceTable,
    pub clients: HashMap<u32, ClientState>,
    pub id_allocator: IdAllocator,
    pub start_instant: Instant,
    pub randr: RandrState,
    /// RANDR event masks selected via RRSelectInput: (client, window) -> mask.
    pub randr_select_masks: HashMap<(u32, ResourceId), u16>,
    /// XKB SelectEvents masks: (client, device spec) -> selected event mask.
    pub xkb_select_event_masks: HashMap<(u32, u16), u16>,
    /// Selection ownership: maps selection atom → (owning window,
    /// `lastTimeChanged` in ms). `lastTimeChanged` is the timestamp
    /// from the `SetSelectionOwner` request that produced this entry;
    /// it surfaces on the wire as `selection_timestamp` in
    /// `XFixesSelectionNotify` events (Xorg `xfixes/select.c:89` reads
    /// `selection->lastTimeChanged`).
    pub selections: HashMap<AtomId, (ResourceId, u32)>,
    /// Active pointer grab: (grab owner, grab window). When set, all pointer
    /// events are redirected to the grab owner regardless of where the cursor is.
    pub pointer_grab: Option<(ClientId, ResourceId)>,
    /// Last known pointer position in root coordinates, cached from the
    /// pointer fanout. XI2 focus events (FocusIn/FocusOut share the
    /// `xXIEnterEvent` layout and carry the pointer position) are emitted
    /// from request handlers that have no backend handle to query the
    /// pointer; without this cache they ship at (0,0).
    pub pointer_root: (i16, i16),
    /// Active pointer grab record (full state including event_mask/cursor/time).
    /// When set, mirrors `pointer_grab` and supersedes it for spec-correct
    /// `ChangeActivePointerGrab` semantics.
    pub active_pointer_grab: Option<ActivePointerGrab>,
    /// Registered passive button grabs.
    pub button_grabs: Vec<PassiveButtonGrab>,
    /// True when `pointer_grab` was activated by a passive button grab.
    pub pointer_grab_is_passive: bool,
    /// Frozen pointer event held by a sync passive grab. This is the
    /// *activating* press that triggered the grab; replayed on
    /// `AllowEvents(ReplayPointer)` / `XIAllowEvents(ReplayDevice)`.
    pub frozen_pointer_event: Option<crate::host_x11::HostPointerEvent>,
    /// Pointer events that arrived while a sync passive grab was frozen
    /// (between the activating press and `AllowEvents`). Mirrors
    /// Xorg's `syncEvents.pending` (`dix/events.c:1320` —
    /// `ComputeFreezes` then `PlayReleasedEvents`). Drained in arrival
    /// order on replay AllowEvents; cleared without delivery on
    /// async/disconnect.
    /// Holding these in a queue (instead of delivering through to the
    /// natural target) is load-bearing for slow-WM cases like MATE's
    /// marco, which does ~10 round-trips of focus/property work
    /// between the press and `AllowEvents(ReplayPointer)` — without
    /// the queue, a fast user release races marco's AllowEvents and
    /// the app sees Release before the replayed Press, malforming the
    /// gesture and breaking menus and titlebar drags.
    pub frozen_pointer_queue: std::collections::VecDeque<crate::host_x11::HostPointerEvent>,
    /// Registered passive key grabs.
    pub key_grabs: Vec<KeyGrab>,
    /// XI 1.x passive device grabs (GrabDeviceKey / GrabDeviceButton).
    pub xi1_passive_grabs: Vec<Xi1PassiveGrab>,
    /// XI 1.x active device grabs, keyed by deviceid. Established by
    /// GrabDevice or by a passive grab activating.
    pub xi1_active_grabs: HashMap<u16, Xi1ActiveGrab>,
    /// XI 1.x last-device-grab time (GrabDevice timestamp validation +
    /// passive-grab activation updates it — XTS XGrabDeviceKey-3).
    pub xi1_last_grab_time: u32,
    /// Core last-pointer-grab time — Xorg `deviceGrab.grabTime` for the
    /// pointer. GrabPointer/UngrabPointer/ChangeActivePointerGrab/
    /// AllowEvents timestamp validation (GrabInvalidTime / no-op).
    pub last_pointer_grab_time: u32,
    /// Core last-keyboard-grab time — Xorg `deviceGrab.grabTime` for
    /// the keyboard (GrabKeyboard/UngrabKeyboard validation).
    pub last_keyboard_grab_time: u32,
    /// Core keyboard focus — Xorg `FocusClassRec` for the virtual core
    /// keyboard. Global across clients (X11 has ONE core focus); the
    /// per-client `focused_window` mirror is kept in sync for legacy
    /// readers.
    pub core_focus: CoreFocus,
    /// Client keysym rows installed via ChangeKeyboardMapping, keyed
    /// by keycode. GetKeyboardMapping merges these over the backend's
    /// keymap (Xorg stores them in the device's key class directly;
    /// the overlay keeps the backend keymap pristine).
    pub keymap_overrides: HashMap<u8, Vec<u32>>,
    /// SetPointerMapping store — logical-button map returned by
    /// GetPointerMapping. None = identity over the core button count.
    pub pointer_mapping_override: Option<Vec<u8>>,
    /// SetModifierMapping store — (keycodes_per_modifier, 8×kpm
    /// keycodes) returned by GetModifierMapping when set.
    pub modifier_mapping_override: Option<(u8, Vec<u8>)>,
    /// Currently-pressed keycodes, one bit per keycode (byte k/8, bit
    /// k%8) — the QueryKeymap bitmap, maintained by the key fanout.
    pub keys_down: [u8; 32],
    /// Window the pointer is currently confined to (0 = none) — set
    /// while an active pointer grab with `confine_to` is in effect
    /// (Xorg `ConfineCursorToWindow`). The pointer fanout clamps
    /// motion to this window's rectangle.
    pub pointer_confine_to: ResourceId,
    /// Currently-pressed pointer buttons, bit b-1 for button b —
    /// fills the button half of the core event state for synthetic
    /// (XTest) events and gates passive-grab activation ("no other
    /// buttons down").
    pub buttons_down: u16,
    /// Re-entrancy guard for the confinement warp: `warp_pointer_root`
    /// synchronously re-enters the pointer fanout with the generated
    /// motion; a coordinate mismatch between the clamp target and the
    /// re-derived event must not warp again (stack overflow).
    pub confine_warp_active: bool,
    /// Most recent input event timestamp seen by either fanout —
    /// stands in for "current server time" in XI1 grab time checks.
    pub xi1_last_input_time: u32,
    /// XI 1.x per-device freeze state for synchronous device grabs
    /// (AllowDeviceEvents). Only the XI1 fanout freezes — core / XI2
    /// delivery is unaffected, bounding the blast radius.
    pub xi1_frozen: HashMap<u16, Xi1Freeze>,
    /// XI 1.x per-device focus (SetDeviceFocus). Missing entry =
    /// device default (PointerRoot / RevertToNone).
    pub xi1_device_focus: HashMap<u16, Xi1DeviceFocus>,
    /// XI 1.x per-device key/button-down bitmasks + valuator mode
    /// (DeviceStateNotify source data). Missing entry = nothing down,
    /// Relative mode.
    pub xi1_device_input_state: HashMap<u16, Xi1DeviceInputState>,
    /// XI 1.x per-window do-not-propagate set
    /// (`XChangeDeviceDontPropagateList`). Each class packs
    /// `(deviceid << 8) | event_code` like every other XI1 class.
    /// `XSendExtensionEvent` honours this set during the propagate
    /// walk — see `xi1_send_extension_event_resolve_targets`
    /// (process_request.rs). The Xorg analogue is
    /// `OtherInputMasks.dontPropagateMask[deviceid]`
    /// (xserver.git Xi/getprop.c, exevents.c:2959).
    pub xi1_window_dont_propagate: HashMap<ResourceId, HashSet<u32>>,
    /// XI 1.x per-device pointer-button mapping
    /// (`XSetDeviceButtonMapping`). Index i (0-based) → physical button
    /// (1-based); 0 disables. Missing entry = identity (i → i+1). Xorg
    /// `Xi/setbmap.c::ProcXSetDeviceButtonMapping` keeps this on
    /// `dev->button->map`; we keep it per-device on the server because
    /// the mapping is server state, not connection state.
    pub xi1_button_map: HashMap<u16, Vec<u8>>,
    /// XI 1.x per-device modifier mapping
    /// (`XSetDeviceModifierMapping`). Stores `numKeyPerModifier` and
    /// the 8 × numKeyPerModifier keycode array. Missing entry =
    /// inherit from the backend keymap. Xorg
    /// `Xi/setmmap.c::ProcXSetDeviceModifierMapping`.
    pub xi1_modifier_map: HashMap<u16, (u8, Vec<u8>)>,
    /// XI 1.x per-device per-axis DEVICE_RESOLUTION control values.
    /// Each row is `[resolution, min_resolution, max_resolution]`
    /// for a single valuator (Xorg `dev->valuator->axes[i]`). Missing
    /// or short entries default to zero — xts5 ChangeDeviceControl
    /// 1 / 2 walk the resolution back through `XGetDeviceControl`.
    pub xi1_resolution: HashMap<u16, Vec<[i32; 3]>>,
    /// Active keyboard grab (explicit or passive-induced).
    pub active_keyboard_grab: Option<ActiveKeyboardGrab>,
    /// Frozen key event held by a sync passive key grab, awaiting
    /// `AllowEvents(ReplayKeyboard)` / `XIAllowEvents(ReplayDevice)`.
    /// Mirrors `frozen_pointer_event`; its presence marks the active
    /// keyboard grab as a synchronous freeze.
    pub frozen_keyboard_event: Option<crate::host_x11::HostKeyEvent>,
    /// XFIXES regions owned by clients.
    pub xfixes_regions: HashMap<u32, XFixesRegion>,
    /// XFIXES selection event masks: (client, window, selection atom) -> mask.
    pub xfixes_selection_masks: HashMap<(u32, ResourceId, AtomId), u32>,
    /// XFIXES cursor event masks: (client, window) -> mask.
    pub xfixes_cursor_masks: HashMap<(u32, ResourceId), u32>,
    /// SHAPE state per window. Missing entries mean the default window rectangle.
    pub shape_windows: HashMap<ResourceId, ShapeWindowState>,
    /// SHAPE select-input state: (client, window) -> enabled.
    pub shape_select_masks: HashMap<(u32, ResourceId), bool>,
    /// Present extension scheduler (Phase 4.2.3). Per-window FIFO of
    /// queued PresentPixmap / PresentPixmapSynced requests. Enqueued
    /// at request time; drained at vblank by the KMS backend
    /// (live integration lands with §5.5 hardware coverage).
    pub present_scheduler: crate::present_scheduler::PresentScheduler,
    pub sync_counters: HashMap<u32, SyncCounter>,
    pub sync_alarms: HashMap<u32, SyncAlarm>,
    /// Per-XI2-master-device idle clock. Key = device id (VCP=2, VCK=3
    /// hard-coded in key_fanout.rs:29 / pointer_fanout.rs:30). Updated
    /// by the fanouts on each input event for the affected device.
    /// `dpms.last_activity` continues to track "any device" — that's the
    /// global IDLETIME baseline.
    pub per_device_last_activity: HashMap<u8, Instant>,
    /// Per-counter cache of the IDLETIME value at the last evaluator
    /// pass. Lets the post-poll evaluator compute `(old, new)`
    /// transitions for `trigger_fires`. Keyed by counter id (one of
    /// `IDLETIME_COUNTER` / `IDLETIME_DEVICE_VCP` / `IDLETIME_DEVICE_VCK`).
    /// Populated by `evaluate_idletime_alarms_post_poll` (Task 4) and the input-wake handler (Task 5).
    pub idletime_last_evaluated: HashMap<u32, i64>,
    /// XSync `Fence` resources (Phase 4.2.2). Phase 4.2.2 first cut
    /// stores only the triggered bit + owner; the underlying
    /// `VkSemaphore` for fences imported via DRI3 `FenceFromFD`
    /// (Task 19) lives on the KMS backend's `dri3_sync_resources` map.
    pub sync_fences: HashMap<u32, SyncFence>,
    pub damage_objects: HashMap<u32, DamageObject>,
    pub composite_redirects: HashMap<(ResourceId, bool), RedirectRecord>,
    pub present_event_selections: HashMap<u32, PresentEventSelection>,
    pub present_msc: HashMap<ResourceId, u64>,
    /// Diagnostic side-table: client_id → first WM_CLASS string the
    /// client set on any of its windows. Used by perf logs to attribute
    /// hot-path activity (e.g. SHM PutImage bursts) to a recognisable
    /// process name. Updated in the WM_CLASS property handler.
    pub client_wm_class: HashMap<u32, String>,
    /// MIT-SHM segments — keyed by client-supplied `shmseg` ID.
    pub mit_shm_segments: HashMap<u32, MitShmSegment>,
    /// GLX context registry. Indirect-rendering clients allocate one
    /// or more contexts via CreateContext / CreateNewContext /
    /// CreateContextAttribsARB; MakeCurrent picks one by XID and
    /// returns a server-issued contextTag the client uses to label
    /// subsequent rendering requests. Direct clients still go through
    /// this path to receive a valid contextTag.
    pub glx_contexts: HashMap<u32, GlxContext>,
    /// Monotonic counter for GLX `contextTag` values returned by
    /// `MakeCurrent` / `MakeContextCurrent`. Tag 0 is reserved by the
    /// protocol to mean "no context current".
    pub glx_next_context_tag: u32,
    /// GLX drawables (windows, pixmaps, pbuffers) — keyed by the GLX
    /// drawable XID the client chose at create-time.
    pub glx_drawables: HashMap<u32, GlxDrawable>,
    /// `GLX_EXT_texture_from_pixmap` is advertised only when the backend
    /// confirmed at init that it can allocate and export a BGRA8 dma-buf.
    /// Set once from `backend.supports_dmabuf_export()` during startup;
    /// read by the GLX string-builder in `process_request.rs`.
    pub glx_tfp_supported: bool,
    /// Server-side key auto-repeat state. Set to `Some` while a key
    /// is held; cleared on the matching release or replaced when a
    /// different key is pressed (X11 spec: only the most recently
    /// pressed key repeats). The core loop's poll uses
    /// `repeat_state.next_fire` to compute its wake-up timeout so an
    /// idle server still costs zero CPU.
    pub repeat_state: Option<KeyRepeatState>,
    /// Global DPMS extension state (power management).
    pub dpms: DpmsState,
    /// MIT-SCREEN-SAVER extension state.
    pub screensaver: ScreenSaverState,
    /// Core keyboard control (ChangeKeyboardControl/GetKeyboardControl).
    pub keyboard_control: KeyboardControlState,
    /// Core pointer control (ChangePointerControl/GetPointerControl).
    pub pointer_control: PointerControlState,
    /// Per-client close-down mode set by `SetCloseDownMode` (opcode 112).
    /// Absent / 0 = Destroy (default); 1 = RetainPermanent; 2 = RetainTemporary.
    /// Only non-zero entries are stored. Read at disconnect time to decide
    /// whether to free or retain the client's resources.
    pub close_down_modes: HashMap<u32, u8>,
    /// Clients whose connection has closed but whose resources are
    /// retained per their final `SetCloseDownMode`. Maps `client_id →
    /// close_mode` (1 = RetainPermanent, 2 = RetainTemporary). Each
    /// zombie's resources keep their original `owner: ClientId` so
    /// `KillClient(resource_id)` can target the exact creator,
    /// not a shared bucket. `KillClient(AllTemporary)` walks zombies
    /// with mode 2 and frees their resources.
    pub zombie_clients: HashMap<u32, u8>,
    /// Outstanding `XSync::AwaitFence` requests waiting on at least
    /// one fence in the list to transition to triggered. Per the
    /// spec the server must defer further processing of the
    /// blocked client's requests until *any* of the listed fences
    /// triggers; **we don't suspend the client's request stream**
    /// (that requires deeper core-loop integration), so this map
    /// only records the await for telemetry + a corresponding
    /// `TriggerFence`-time `AwaitSatisfied` debug log. Real
    /// blocking is left as a known gap — see followup §5 in
    /// `docs/superpowers/specs/2026-05-09-phase4-2-dri3-present-glx-design.md`.
    pub sync_pending_awaits: Vec<SyncPendingAwait>,
    /// Cumulative XI2 scroll-axis values for the master pointer.
    /// `[0]` is valuator number 2 (vertical scroll), `[1]` is
    /// valuator number 3 (horizontal scroll). Increments by 1 per
    /// logical wheel click, matching the `increment=1.0` advertised
    /// on the XIScrollClass entries in the XIQueryDevice reply. GDK
    /// reads the cumulative value off each XI_Motion-with-scroll-
    /// axis event and computes deltas from the previous sample.
    pub scroll_axis_value: [i32; 2],
    /// Installed colormaps in install order (oldest first). Capacity
    /// is the server's max installed minimum; we only have a single
    /// hardware colormap (TrueColor) so the list mostly mirrors the
    /// current focus colormap. Read by `ListInstalledColormaps` and
    /// mutated by `InstallColormap` / `UninstallColormap`. Seeded with
    /// `ROOT_COLORMAP` at startup per X11 spec ("the default colormap
    /// for the screen is installed when the server first starts up").
    pub installed_colormaps: Vec<ResourceId>,
    /// XI2 device and property registry.  One entry per static XI2
    /// device (ids 2–5, mirroring the XIQueryDevice reply).  The slave-
    /// pointer entry (id 4) is updated by `xi_seed_touchpad` /
    /// `xi_clear_touchpad` when libinput reports a touchpad device.
    /// Read by the XIListProperties / XIGetProperty handlers.
    pub xi_devices: Vec<crate::xinput::XiDevice>,
    /// Pre-interned atom for the property-type literal `"FLOAT"`.
    ///
    /// `FLOAT` is **not** a predefined X atom, so the libinput
    /// `Accel Speed` family of properties — which Xorg types
    /// `FLOAT/32` — needs us to intern the name at startup so the
    /// type-atom on the wire never resolves to id 0.
    pub float_atom: AtomId,
}

/// Server-side key auto-repeat. Carries the original `HostKeyEvent`
/// so synthetic repeat events can re-use its time/state/coord
/// fields when fan-out runs, plus the `Instant` at which the next
/// repeat should fire. Per-key delay/rate overrides aren't tracked
/// today — `core_loop::run` uses the X11 defaults (660 ms initial,
/// 40 ms period ≈ 25 Hz).
#[derive(Clone, Copy, Debug)]
pub struct KeyRepeatState {
    pub event: crate::host_x11::HostKeyEvent,
    pub next_fire: std::time::Instant,
}

/// One outstanding `XSync::AwaitFence` request that hasn't been
/// satisfied yet. Stored on `ServerState` until any fence in
/// `fences` triggers.
#[derive(Clone, Debug)]
pub struct SyncPendingAwait {
    pub client: ClientId,
    pub sequence: SequenceNumber,
    pub fences: Vec<u32>,
}

/// GLX context resource. We never run server-side GL — direct-
/// rendering clients use the tag we assign at MakeCurrent to label
/// rendering requests, but no actual GL state is tracked here.
#[derive(Clone, Debug)]
pub struct GlxContext {
    pub owner: ClientId,
    pub fbconfig: u32,
    pub render_type: u32,
}

/// GLX drawable resource — `CreateGLXWindow` / `CreateGLXPixmap` /
/// `CreatePbuffer` allocations. Stores the bound X drawable, the
/// FBConfig the client picked, and the attributes Mesa later reads
/// back via `GetDrawableAttributes`.
#[derive(Clone, Debug)]
pub struct GlxDrawable {
    pub owner: ClientId,
    pub x_drawable: u32,
    pub fbconfig: u32,
    /// Drawable size. For pbuffers this is the `GLX_PBUFFER_WIDTH`/`HEIGHT`
    /// from `CreatePbuffer`, reported back via `GetDrawableAttributes` as
    /// `GLX_WIDTH`/`GLX_HEIGHT` so Mesa can size the buffer. 0 for
    /// window/pixmap drawables (Mesa sizes those from the X drawable /
    /// `Present::ConfigureNotify`).
    pub width: u32,
    pub height: u32,
    pub attributes: Vec<(u32, u32)>,
    /// host_xid resolved at `glXCreatePixmap` acquire time, so release is
    /// robust to the X pixmap being freed first. Without this, a client
    /// calling X11 `FreePixmap` before `glXDestroyPixmap` (a common
    /// compositor ordering) would leave the release sites unable to
    /// re-resolve `x_drawable → host_xid` (the resource is gone), leaking
    /// the export ref forever. `None` for the window/pbuffer cases.
    pub glx_export_host_xid: Option<u32>,
}

impl ServerState {
    #[must_use]
    pub fn new() -> Self {
        Self::with_geometry(800, 600)
    }

    #[must_use]
    pub fn with_geometry(width: u16, height: u16) -> Self {
        let mut resources = ResourceTable::new();
        if let Some(root) = resources.window_mut(ROOT_WINDOW) {
            root.width = width;
            root.height = height;
        }
        if let Some(overlay) = resources.window_mut(COMPOSITE_OVERLAY_WINDOW) {
            overlay.width = width;
            overlay.height = height;
        }
        let mut atoms = AtomTable::new();
        // Intern the XI 1.x device-type atoms at server startup so that
        // clients calling InternAtom(only_if_exists=true) at session init
        // (e.g. MATE's settings daemon checking for "TOUCHPAD") find them
        // before calling XListInputDevices.
        atoms.intern(crate::xinput::XI_ATOM_MOUSE, false);
        atoms.intern(crate::xinput::XI_ATOM_KEYBOARD, false);
        atoms.intern(crate::xinput::XI_ATOM_TOUCHPAD, false);
        // FLOAT is not a predefined X atom; intern it now so the
        // libinput accel-speed property family can stamp
        // type=float_atom on its wire replies without a per-request
        // intern dance.
        let float_atom = atoms.intern("FLOAT", false);
        Self {
            atoms,
            resources,
            clients: HashMap::new(),
            id_allocator: IdAllocator::new(),
            start_instant: Instant::now(),
            randr: RandrState::nested(0, width, height),
            randr_select_masks: HashMap::new(),
            xkb_select_event_masks: HashMap::new(),
            selections: HashMap::new(),
            pointer_grab: None,
            pointer_root: (0, 0),
            active_pointer_grab: None,
            button_grabs: Vec::new(),
            pointer_grab_is_passive: false,
            frozen_pointer_queue: std::collections::VecDeque::new(),
            frozen_pointer_event: None,
            key_grabs: Vec::new(),
            xi1_passive_grabs: Vec::new(),
            xi1_active_grabs: HashMap::new(),
            xi1_last_grab_time: 0,
            last_pointer_grab_time: 0,
            last_keyboard_grab_time: 0,
            core_focus: CoreFocus::default(),
            keymap_overrides: HashMap::new(),
            pointer_mapping_override: None,
            modifier_mapping_override: None,
            keys_down: [0u8; 32],
            pointer_confine_to: ResourceId(0),
            buttons_down: 0,
            confine_warp_active: false,
            xi1_last_input_time: 0,
            xi1_frozen: HashMap::new(),
            xi1_device_focus: HashMap::new(),
            xi1_device_input_state: HashMap::new(),
            xi1_window_dont_propagate: HashMap::new(),
            xi1_button_map: HashMap::new(),
            xi1_modifier_map: HashMap::new(),
            xi1_resolution: HashMap::new(),
            active_keyboard_grab: None,
            frozen_keyboard_event: None,
            xfixes_regions: HashMap::new(),
            xfixes_selection_masks: HashMap::new(),
            xfixes_cursor_masks: HashMap::new(),
            shape_windows: HashMap::new(),
            shape_select_masks: HashMap::new(),
            present_scheduler: crate::present_scheduler::PresentScheduler::default(),
            sync_counters: HashMap::new(),
            sync_alarms: HashMap::new(),
            per_device_last_activity: HashMap::new(),
            idletime_last_evaluated: HashMap::new(),
            sync_fences: HashMap::new(),
            damage_objects: HashMap::new(),
            composite_redirects: HashMap::new(),
            present_event_selections: HashMap::new(),
            present_msc: HashMap::new(),
            client_wm_class: HashMap::new(),
            mit_shm_segments: HashMap::new(),
            glx_contexts: HashMap::new(),
            glx_next_context_tag: 1,
            glx_drawables: HashMap::new(),
            glx_tfp_supported: false,
            sync_pending_awaits: Vec::new(),
            repeat_state: None,
            dpms: DpmsState::new(false),
            screensaver: ScreenSaverState::new(),
            keyboard_control: KeyboardControlState::new(),
            pointer_control: PointerControlState::new(),
            close_down_modes: HashMap::new(),
            zombie_clients: HashMap::new(),
            scroll_axis_value: [0; 2],
            installed_colormaps: vec![crate::resources::ROOT_COLORMAP],
            xi_devices: crate::xinput::initial_xi_devices(),
            float_atom,
        }
    }

    /// Build a `ServerState` seeded with a caller-supplied set of
    /// RANDR outputs (e.g. from `KmsBackend::randr_outputs`). The
    /// aggregated screen extent from `outputs` overrides `width` /
    /// `height` for the root window when non-zero.
    #[must_use]
    pub fn with_randr_outputs(width: u16, height: u16, outputs: Vec<RandrOutput>) -> Self {
        let mut s = Self::with_geometry(width, height);
        s.randr = RandrState::from_outputs(0, outputs);
        // Re-apply aggregated screen extent to root window if outputs
        // imply a different size than the (width, height) args.
        if let Some(root) = s.resources.window_mut(ROOT_WINDOW) {
            root.width = s.randr.screen_width;
            root.height = s.randr.screen_height;
        }
        if let Some(overlay) = s.resources.window_mut(COMPOSITE_OVERLAY_WINDOW) {
            overlay.width = s.randr.screen_width;
            overlay.height = s.randr.screen_height;
        }
        s
    }

    /// Seed the XI2 device-property registry from a libinput touchpad
    /// device-add event.
    ///
    /// When `info.is_touchpad` is true, the slave-pointer entry (id 4)
    /// receives the real device name and a set of libinput-style
    /// properties.  Non-touchpad devices are silently ignored.
    ///
    /// Property-name atoms are interned via `self.atoms` so they share
    /// the same atom namespace as all other server atoms.
    pub fn xi_seed_touchpad(&mut self, info: &crate::core_loop::DeviceInfo) {
        if !info.is_touchpad {
            return;
        }
        crate::xinput::seed_touchpad(&mut self.xi_devices, &mut self.atoms, self.float_atom, info);
    }

    /// Revert the slave-pointer XI2 device entry to its generic defaults
    /// and clear all touchpad properties.
    ///
    /// Called from `on_host_input(DeviceRemoved)`.  `device_node` is
    /// used for logging; no node→device mapping is maintained today
    /// (one touchpad assumed).
    pub fn xi_clear_touchpad(&mut self, device_node: &str) {
        crate::xinput::clear_touchpad(&mut self.xi_devices, device_node);
    }

    #[must_use]
    pub fn timestamp_now(&self) -> u32 {
        // X11 timestamps are 32-bit milliseconds; truncation is intentional.
        let elapsed = self.start_instant.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let ts = elapsed as u32;
        ts
    }

    /// Baseline `Instant` for an IDLETIME-family counter. Falls back to
    /// `dpms.last_activity` (global) for unknown counters so that a
    /// per-device counter query before any device-specific input has
    /// landed still returns a sensible "any device" idle.
    #[must_use]
    pub fn idletime_baseline(&self, counter: u32) -> Instant {
        use yserver_protocol::x11::sync as x11sync;
        match counter {
            x11sync::IDLETIME_DEVICE_VCP => self
                .per_device_last_activity
                .get(&2)
                .copied()
                .unwrap_or(self.dpms.last_activity),
            x11sync::IDLETIME_DEVICE_VCK => self
                .per_device_last_activity
                .get(&3)
                .copied()
                .unwrap_or(self.dpms.last_activity),
            // Global IDLETIME (and any unknown counter routed here).
            _ => self.dpms.last_activity,
        }
    }

    /// Earliest instant any Active IDLETIME alarm could fire from idle
    /// progression alone. Negative-* alarms fire on input wake (handled
    /// by the fanouts), so they don't contribute to this deadline.
    /// Returns `None` when no eligible alarm exists, or when
    /// `XScreenSaverSuspend` has gated the unified timer (Xorg
    /// WaitFor.c:519).
    ///
    /// **Quiescent-state handling.** A `PositiveTransition + delta=0`
    /// alarm that has already fired stays Active but is *quiescent*
    /// until the counter drops below `wait_value` and crosses up again
    /// (which requires an input event resetting `last_activity`). For
    /// such alarms, the deadline only contributes when current idle is
    /// strictly below `wait_value`. Without this check the poll-min
    /// would lock at a past `Instant` forever and spin with
    /// `Duration::ZERO`. `PositiveComparison` is level-triggered: a
    /// `delta=0` Comparison transitions to Inactive on fire (Xorg
    /// `sync.c:548-555`) so it never re-enters this path; a
    /// `delta != 0` Comparison re-arms `wait_value` past the current
    /// value, so by construction `current_idle < wait_value` and the
    /// deadline is in the future.
    #[must_use]
    pub fn idletime_alarm_deadline(&self) -> Option<std::time::Instant> {
        use yserver_protocol::x11::sync as x11sync;
        if !self.screensaver.suspend_counts.is_empty() {
            return None;
        }
        let now = std::time::Instant::now();
        let mut earliest: Option<std::time::Instant> = None;
        for alarm in self.sync_alarms.values() {
            if alarm.state != x11sync::ALARM_STATE_ACTIVE {
                continue;
            }
            if !matches!(
                alarm.counter,
                x11sync::IDLETIME_COUNTER
                    | x11sync::IDLETIME_DEVICE_VCP
                    | x11sync::IDLETIME_DEVICE_VCK
            ) {
                continue;
            }
            let test_type = u32::from(alarm.test_type);
            if !matches!(
                test_type,
                x11sync::TEST_POSITIVE_TRANSITION | x11sync::TEST_POSITIVE_COMPARISON
            ) {
                continue;
            }
            if alarm.wait_value < 0 {
                continue; // negative wait_value can't be reached by idle (unsigned ms)
            }
            let baseline = self.idletime_baseline(alarm.counter);
            // Quiescent-state skip: drop alarms whose threshold is already
            // at-or-below current idle. They've already fired (Transition)
            // or would re-fire every poll (Comparison) — neither shape
            // contributes a future-instant deadline. They re-enter the
            // deadline only after an input event resets `baseline`, at
            // which point `current_idle < wait_value` again.
            #[allow(clippy::cast_possible_truncation)]
            let current_idle_ms = now
                .duration_since(baseline)
                .as_millis()
                .min(u128::from(u32::MAX)) as i64;
            if current_idle_ms >= alarm.wait_value {
                continue;
            }
            #[allow(clippy::cast_sign_loss)]
            let fire_at = baseline + std::time::Duration::from_millis(alarm.wait_value as u64);
            earliest = Some(earliest.map_or(fire_at, |e| e.min(fire_at)));
        }
        earliest
    }

    /// Earliest instant a DPMS transition could fire. Picks the
    /// smallest non-zero timeout strictly above the current level.
    /// Returns `None` when DPMS is off (either `!enabled` or
    /// `!kms_capable`), when a client has suspended via
    /// `XScreenSaverSuspend` (Xorg `WaitFor.c:519` unified-timer rule),
    /// when there is no higher level to reach, or when every higher
    /// level's timeout is zero (disabled).
    #[must_use]
    pub fn dpms_transition_deadline(&self) -> Option<std::time::Instant> {
        if !self.dpms.enabled || !self.dpms.kms_capable {
            return None;
        }
        // Xorg WaitFor.c:519 — single timer drives both SS and DPMS,
        // not armed when screenSaverSuspended. XScreenSaverSuspend
        // inhibits BOTH the SS timer and the DPMS cascade (mpv /
        // Firefox / vlc rely on this for fullscreen-video-inhibit).
        if !self.screensaver.suspend_counts.is_empty() {
            return None;
        }
        let mut next: Option<u32> = None;
        let mut push = |ms: u32| {
            if ms > 0 {
                next = Some(next.map_or(ms, |n| n.min(ms)));
            }
        };
        let lvl = self.dpms.power_level;
        if lvl < 1 {
            push(self.dpms.standby_ms);
        }
        if lvl < 2 {
            push(self.dpms.suspend_ms);
        }
        if lvl < 3 {
            push(self.dpms.off_ms);
        }
        Some(self.dpms.last_activity + std::time::Duration::from_millis(u64::from(next?)))
    }

    /// Instant the SS idle timer should fire next. None when:
    /// - the timer is disabled (`timeout_ms == 0`),
    /// - a client has suspended via `XScreenSaverSuspend`,
    /// - SS is already active, or
    /// - DPMS has already blanked the panel (Xorg `WaitFor.c:457` —
    ///   the DPMS→SS coupling already handled it; firing the idle
    ///   timer now would be a redundant no-op transition).
    #[must_use]
    pub fn screensaver_idle_deadline(&self) -> Option<std::time::Instant> {
        if self.screensaver.timeout_ms == 0
            || !self.screensaver.suspend_counts.is_empty()
            || matches!(self.screensaver.active, ScreenSaverActive::On)
            || self.dpms.power_level != 0
        {
            return None;
        }
        Some(
            self.dpms.last_activity
                + std::time::Duration::from_millis(u64::from(self.screensaver.timeout_ms)),
        )
    }

    /// Instant the next `ScreenSaverNotify(state=Cycle)` should fire.
    /// None when SS is Off, when a client has suspended, when DPMS
    /// has blanked, or when `next_cycle` is `None` (no cycle
    /// scheduled — `interval_ms == 0` at the activation transition).
    #[must_use]
    pub fn screensaver_cycle_deadline(&self) -> Option<std::time::Instant> {
        if !matches!(self.screensaver.active, ScreenSaverActive::On)
            || !self.screensaver.suspend_counts.is_empty()
            || self.dpms.power_level != 0
        {
            return None;
        }
        self.screensaver.next_cycle
    }

    /// True iff `id` designates ANY live resource in ANY client-XID
    /// namespace: the 8 ResourceTable maps (via
    /// `resources.xid_in_use`) plus the 9 extension maps that
    /// `xid_in_use` does NOT cover. XC-MISC must never report an
    /// occupied id as free — extend this (and the
    /// `xid_occupied_covers_every_namespace` test) when adding an
    /// XID-keyed map. Maps keyed by client ids / host xids /
    /// internal handles do NOT belong here.
    #[must_use]
    pub fn xid_occupied(&self, id: u32) -> bool {
        self.resources.xid_in_use(ResourceId(id))
            || self.xfixes_regions.contains_key(&id)
            || self.sync_counters.contains_key(&id)
            || self.sync_alarms.contains_key(&id)
            || self.sync_fences.contains_key(&id)
            || self.damage_objects.contains_key(&id)
            || self.mit_shm_segments.contains_key(&id)
            || self.glx_contexts.contains_key(&id)
            || self.glx_drawables.contains_key(&id)
            || self.present_event_selections.contains_key(&id)
    }

    /// Sorted, deduped list of occupied XIDs in `base..=base|mask`
    /// across all 17 namespaces. O(total live resources) — called
    /// only on the rare XC-MISC GetXIDRange path.
    #[must_use]
    pub fn used_xids_in(&self, base: u32, mask: u32) -> Vec<u32> {
        let mut out = Vec::new();
        self.resources.collect_xids_in(base, mask, &mut out);
        let in_range = |id: &&u32| (**id & !mask) == base;
        out.extend(self.xfixes_regions.keys().filter(in_range));
        out.extend(self.sync_counters.keys().filter(in_range));
        out.extend(self.sync_alarms.keys().filter(in_range));
        out.extend(self.sync_fences.keys().filter(in_range));
        out.extend(self.damage_objects.keys().filter(in_range));
        out.extend(self.mit_shm_segments.keys().filter(in_range));
        out.extend(self.glx_contexts.keys().filter(in_range));
        out.extend(self.glx_drawables.keys().filter(in_range));
        out.extend(self.present_event_selections.keys().filter(in_range));
        out.sort_unstable();
        out.dedup();
        out
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XFixesRegion {
    pub owner: ClientId,
    pub rects: Vec<xfixes::RegionRect>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShapeWindowState {
    pub bounding: Option<Vec<xfixes::RegionRect>>,
    pub clip: Option<Vec<xfixes::RegionRect>>,
    pub input: Option<Vec<xfixes::RegionRect>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncCounter {
    pub owner: ClientId,
    pub value: i64,
}

/// XSync `Fence` resource. Phase 4.2.2 first cut: server-only
/// triggered bit; the VkSemaphore-backed variant is added by Task 19
/// when `FenceFromFD` imports a sync_file fd. Both flavours share the
/// `triggered` field so QueryFence has a uniform answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncFence {
    pub owner: ClientId,
    pub triggered: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncAlarm {
    pub owner: ClientId,
    pub counter: u32,
    /// Absolute counter value the trigger tests against. For a Relative
    /// alarm this is resolved at create/change time (counter + value).
    pub wait_value: i64,
    pub delta: i64,
    /// `XSyncTestType` (PositiveTransition=0 … NegativeComparison=3).
    pub test_type: u8,
    pub events: bool,
    /// `XSyncAlarmState` (Active=0, Inactive=1, Destroyed=2).
    pub state: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DamageObject {
    pub owner: ClientId,
    pub drawable: ResourceId,
    pub level: u8,
    pub rects: Vec<xfixes::RegionRect>,
    /// True when we've already emitted a `DamageNotify` for this Subtract
    /// cycle. The "cycle" begins after `DamageSubtract` clears the
    /// accumulated region. Levels 2 (BoundingBox) and 3 (NonEmpty) emit at
    /// most one event per cycle; resetting this flag is the cycle boundary.
    pub pending_notify_fired: bool,
    /// Geometry carried on the most recently emitted `DamageNotify`.
    /// Coalesced DAMAGE levels report only one notify per cycle, but
    /// window moves/resizes change the `geometry` payload even when
    /// the area is still coalesced. Tracking the last report lets us
    /// emit a follow-up notify when geometry changes mid-cycle.
    pub last_reported_geometry: Option<x11::damage::Rectangle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentEventSelection {
    pub owner: ClientId,
    pub window: ResourceId,
    pub event_mask: u32,
}

/// A shared memory segment attached via MIT-SHM. Owns the lifetime of both
/// the file descriptor and the kernel mapping; the `Drop` impl `munmap`s
/// and closes the fd in the right order.
#[derive(Debug)]
pub struct MitShmSegment {
    pub owner: ClientId,
    /// Length of the memory mapping, in bytes.
    pub size: usize,
    /// Whether the client requested a read-only attach. We honour this on
    /// `GetImage` (which writes back into the segment) by failing those
    /// requests with `BadAccess`.
    pub read_only: bool,
    /// Pointer to the start of the mapping. Always non-null while this
    /// `MitShmSegment` is alive.
    addr: *mut libc::c_void,
    /// Backing source — either an FD we close on Drop, or a SysV shmat
    /// mapping we shmdt on Drop.
    backing: MitShmBacking,
}

#[derive(Debug)]
enum MitShmBacking {
    /// `AttachFd`: file descriptor that backs `addr` via `mmap`.
    Fd(libc::c_int),
    /// Legacy `Attach`: SysV mapping. `addr` was returned by `shmat(2)`.
    Sysv,
}

// Safe to send across threads: the underlying memory is independent of any
// thread-local state, and we serialize access via `&mut ServerState`.
unsafe impl Send for MitShmSegment {}
unsafe impl Sync for MitShmSegment {}

impl MitShmSegment {
    /// Map an attached file descriptor. Caller must have verified the FD
    /// references a regular file or memfd; we `fstat` to learn the size.
    ///
    /// On success, takes ownership of `fd` and will close it on `Drop`.
    pub fn from_fd(owner: ClientId, fd: libc::c_int, read_only: bool) -> std::io::Result<Self> {
        // Stat to get size.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut stat) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        if stat.st_size <= 0 {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MIT-SHM AttachFd: zero-length segment",
            ));
        }
        let size = stat.st_size as usize;
        let prot = if read_only {
            libc::PROT_READ
        } else {
            libc::PROT_READ | libc::PROT_WRITE
        };
        let addr = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, libc::MAP_SHARED, fd, 0) };
        if addr == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self {
            owner,
            size,
            read_only,
            addr,
            backing: MitShmBacking::Fd(fd),
        })
    }

    /// Attach a SysV shared-memory segment created by the client via
    /// `shmget(2)`. Returns an error when the kernel rejects `shmat` (most
    /// commonly because the client and server live in different IPC
    /// namespaces).
    pub fn from_shmid(owner: ClientId, shmid: u32, read_only: bool) -> std::io::Result<Self> {
        let flags = if read_only { libc::SHM_RDONLY } else { 0 };
        // SAFETY: shmat is fine to call with arbitrary user-provided shmid;
        // it returns (void*)-1 on failure.
        let addr = unsafe { libc::shmat(shmid as libc::c_int, std::ptr::null(), flags) };
        if addr == (-1_isize as *mut libc::c_void) {
            return Err(std::io::Error::last_os_error());
        }
        // Query the segment size via shmctl(IPC_STAT).
        let mut info: libc::shmid_ds = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::shmctl(shmid as libc::c_int, libc::IPC_STAT, &raw mut info) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::shmdt(addr) };
            return Err(err);
        }
        let size = info.shm_segsz as usize;
        if size == 0 {
            unsafe { libc::shmdt(addr) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MIT-SHM Attach: zero-length segment",
            ));
        }
        Ok(Self {
            owner,
            size,
            read_only,
            addr,
            backing: MitShmBacking::Sysv,
        })
    }

    /// View into the mapped memory. Lifetime is tied to `&self`, so callers
    /// may only borrow within their own request handler.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `addr` is non-null and valid for `size` bytes for the
        // lifetime of `self` (until Drop). Memory is shared across processes
        // but X server semantics expect us to read the snapshot from our
        // perspective synchronously inside a request handler.
        unsafe { std::slice::from_raw_parts(self.addr.cast::<u8>(), self.size) }
    }

    /// Mutable view into the mapped memory. Returns `None` for read-only
    /// segments — caller should reply `BadAccess`.
    pub fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        if self.read_only {
            return None;
        }
        // SAFETY: same as `as_slice`, plus the segment was mapped with
        // `PROT_WRITE` because `read_only == false`.
        Some(unsafe { std::slice::from_raw_parts_mut(self.addr.cast::<u8>(), self.size) })
    }
}

impl Drop for MitShmSegment {
    fn drop(&mut self) {
        unsafe {
            match self.backing {
                MitShmBacking::Fd(fd) => {
                    libc::munmap(self.addr, self.size);
                    libc::close(fd);
                }
                MitShmBacking::Sysv => {
                    libc::shmdt(self.addr);
                }
            }
        }
    }
}

impl Default for SyncAlarm {
    fn default() -> Self {
        Self {
            owner: ClientId(0),
            counter: 0,
            wait_value: 0,
            delta: 0,
            test_type: 0,
            events: false,
            state: 0,
        }
    }
}

impl ShapeWindowState {
    pub fn rects_mut(&mut self, kind: u8) -> Option<&mut Option<Vec<xfixes::RegionRect>>> {
        match kind {
            shape::KIND_BOUNDING => Some(&mut self.bounding),
            shape::KIND_CLIP => Some(&mut self.clip),
            shape::KIND_INPUT => Some(&mut self.input),
            _ => None,
        }
    }

    #[must_use]
    pub fn rects(&self, kind: u8) -> Option<&Vec<xfixes::RegionRect>> {
        match kind {
            shape::KIND_BOUNDING => self.bounding.as_ref(),
            shape::KIND_CLIP => self.clip.as_ref(),
            shape::KIND_INPUT => self.input.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct ClientState {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub event_masks: HashMap<ResourceId, u32>,
    /// Foreign windows the client wants kept alive after disconnect
    /// (X11 ChangeSaveSet semantics).
    pub save_set: HashSet<ResourceId>,
    pub big_requests_enabled: bool,
    /// XI2 event masks: (window_id, device_id) -> mask
    pub xi2_masks: HashMap<(ResourceId, u16), u32>,
    /// XI1 `XEventClass` values the client has selected via
    /// `SelectExtensionEvent` (XInput minor 6). Each class encodes
    /// `(deviceid << 8) | event_code` where `event_code` is one of the
    /// 17 XInput event types at `XI_FIRST_EVENT..=XI_FIRST_EVENT + 16`.
    /// Classes are stored verbatim — the XI1 events delivered from this
    /// set (`DevicePropertyNotify`) are device-scoped, not
    /// window-scoped, so the request's `window` argument is ignored.
    pub xi1_event_classes: HashSet<u32>,
    /// XI1 *input*-event classes per window — DeviceKeyPress through
    /// DeviceMotionNotify select like core input events: per window,
    /// delivered by walking the event window's ancestor chain
    /// (Xorg dix `DeliverDeviceEvents`). Key: window; value: the
    /// selected `XEventClass` values for it.
    pub xi1_window_event_classes: HashMap<ResourceId, HashSet<u32>>,
    /// Outbound bytes buffered when the client write fd would block.
    /// Populated in D2.
    pub outbound: std::collections::VecDeque<u8>,
    /// Whether the core's mio poller currently watches this client's
    /// writer fd for WRITABLE.  Used in I2.
    pub watching_writable: bool,
    /// Window the client's pointer/key events route through; demoted off
    /// `Arc<Mutex<ResourceId>>` in D3.
    pub focused_window: ResourceId,
    /// Control channel to the per-client reader thread; populated in D4
    /// when the reader is spawned.
    pub reader_control: Option<crossbeam_channel::Sender<ReaderControl>>,
}

/// Messages the core sends to a per-client reader thread.
///
/// `Apply`/`Ignore` are the BigRequests barrier: the reader pauses
/// after sending an Enable request and resumes once the core processed
/// it. `Shutdown` causes the reader to exit (also unparks any reader
/// blocked on a barrier).
#[derive(Debug)]
pub enum ReaderControl {
    /// Enable was processed; reader resumes with `big = true`.
    ApplyBigRequests,
    /// Enable was malformed or the reply path errored; reader
    /// resumes with the previous `big` value.
    IgnoreBigRequests,
    Shutdown,
}

/// Snapshot of a client's writer for cross-client event fanout.
#[derive(Clone)]
pub struct EventTarget {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
}

impl ServerState {
    /// Stage 4e — set the COW's input shape to empty (click-through) at
    /// materialization. Mirrors Xorg's compositor convention where the
    /// COW's default input region passes pointer events through to
    /// underlying root children, with descendants like the compositor's
    /// stage receiving input directly.
    ///
    /// Pairs with `ResourceTable::materialize_cow_resource` — both run
    /// from the `GetOverlayWindow` handler on the 0→1 transition.
    pub fn materialize_cow_input_shape(&mut self) {
        self.shape_windows
            .entry(COMPOSITE_OVERLAY_WINDOW)
            .or_default()
            .input = Some(Vec::<xfixes::RegionRect>::new());
    }

    /// Symmetric teardown for [`Self::materialize_cow_input_shape`]. Called
    /// from the `ReleaseOverlayWindow` handler on the 1→0 transition.
    pub fn destroy_cow_input_shape(&mut self) {
        self.shape_windows.remove(&COMPOSITE_OVERLAY_WINDOW);
    }

    fn event_target_for_client(client: &ClientState) -> EventTarget {
        EventTarget {
            writer: client.writer.clone(),
            byte_order: client.byte_order,
            last_sequence: client.last_sequence.clone(),
        }
    }

    #[must_use]
    pub fn pointer_target_at(
        &self,
        top_level: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        let top = self.resources.window(top_level)?;
        if top.map_state == crate::resources::MapState::Unmapped {
            return None;
        }
        let mut best = (top_level, x, y);
        self.pointer_target_at_inner(top_level, x, y, &mut best);
        Some(best)
    }

    #[must_use]
    pub fn root_pointer_target_at(&self, x: i16, y: i16) -> Option<(ResourceId, i16, i16)> {
        self.pointer_target_at(ROOT_WINDOW, x, y)
    }

    #[must_use]
    pub fn direct_child_at(&self, parent: ResourceId, x: i16, y: i16) -> Option<ResourceId> {
        self.hit_test_children(parent, x, y)
            .map(|(child, _, _)| child)
    }

    #[must_use]
    pub fn top_level_for_target(&self, target: ResourceId) -> ResourceId {
        let mut current = target;
        let mut top = target;
        for _ in 0..256 {
            let Some(window) = self.resources.window(current) else {
                break;
            };
            if window.parent == current || window.parent == ROOT_WINDOW {
                return top;
            }
            top = window.parent;
            current = window.parent;
        }
        top
    }

    fn pointer_target_at_inner(
        &self,
        parent: ResourceId,
        parent_x: i16,
        parent_y: i16,
        best: &mut (ResourceId, i16, i16),
    ) {
        let Some((child_id, child_x, child_y)) = self.hit_test_children(parent, parent_x, parent_y)
        else {
            return;
        };
        *best = (child_id, child_x, child_y);
        self.pointer_target_at_inner(child_id, child_x, child_y, best);
    }

    fn hit_test_children(
        &self,
        parent: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        // Strict-Xorg miSpriteTrace: iterate children top-to-bottom and
        // let hit_test_child's window_input_contains gate decide each one.
        // The COW is no longer special once it's a real root child
        // (Phase 2 materialization). With its default empty input shape,
        // hit_test_child(COW) returns None and the trace continues to
        // the next sibling — exactly matching Xorg's mi/misprite.c.
        // When a compositor populates the COW input region via XFIXES,
        // the gate descends naturally via pointer_target_at_inner's
        // recursive walk.
        let parent_window = self.resources.window(parent)?;
        for child_id in parent_window.children.iter().rev() {
            if let Some(hit) = self.hit_test_child(*child_id, x, y) {
                return Some(hit);
            }
        }
        None
    }

    fn hit_test_child(
        &self,
        child_id: ResourceId,
        parent_x: i16,
        parent_y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        let child = self.resources.window(child_id)?;
        if child.map_state == crate::resources::MapState::Unmapped {
            return None;
        }
        let child_x = parent_x.wrapping_sub(child.x);
        let child_y = parent_y.wrapping_sub(child.y);
        if child_x < 0
            || child_y < 0
            || child_x >= i16::try_from(child.width).unwrap_or(i16::MAX)
            || child_y >= i16::try_from(child.height).unwrap_or(i16::MAX)
            || !self.window_input_contains(child_id, child_x, child_y)
        {
            return None;
        }
        Some((child_id, child_x, child_y))
    }

    fn window_input_contains(&self, window: ResourceId, x: i16, y: i16) -> bool {
        let Some(rects) = self
            .shape_windows
            .get(&window)
            .and_then(|state| state.input.as_ref())
        else {
            return true;
        };
        rects.iter().any(|rect| {
            let rx = i32::from(rect.x);
            let ry = i32::from(rect.y);
            let rr = rx + i32::from(rect.width);
            let rb = ry + i32::from(rect.height);
            let px = i32::from(x);
            let py = i32::from(y);
            px >= rx && py >= ry && px < rr && py < rb
        })
    }

    #[must_use]
    pub fn subscribers(&self, window: ResourceId, mask_bit: u32) -> Vec<EventTarget> {
        self.clients
            .values()
            .filter_map(|c| {
                let mask = c.event_masks.get(&window).copied().unwrap_or(0);
                if mask & mask_bit != 0 {
                    Some(Self::event_target_for_client(c))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Walk up the parent chain from `start`, returning the first window with
    /// any client subscribed to `mask_bit`, the (event_x, event_y) translated
    /// to be relative to that window, and the subscriber list.
    ///
    /// Per X11 protocol, device events propagate up the window tree until a
    /// client has the event selected on the chain. Used for ButtonPress,
    /// ButtonRelease, MotionNotify. Without this walk, a click on a window
    /// that doesn't subscribe (e.g. e16's "Root-bg" cover window over the
    /// root) is dropped instead of bubbling to root where the WM listens.
    #[must_use]
    pub fn pointer_propagation_target(
        &self,
        start: ResourceId,
        start_x: i16,
        start_y: i16,
        mask_bit: u32,
    ) -> Option<(ResourceId, i16, i16, Vec<EventTarget>)> {
        let mut current = start;
        let mut x = start_x;
        let mut y = start_y;
        for _ in 0..256 {
            let subs = self.subscribers(current, mask_bit);
            if !subs.is_empty() {
                return Some((current, x, y, subs));
            }
            let window = self.resources.window(current)?;
            // Root's parent points to itself; stop after probing root.
            if window.parent == current {
                return None;
            }
            // Translate (x, y) from current-relative to parent-relative.
            x = x.wrapping_add(window.x);
            y = y.wrapping_add(window.y);
            current = window.parent;
        }
        None
    }

    #[must_use]
    pub fn subscribers_intersecting(
        &self,
        window: ResourceId,
        event_mask: u32,
    ) -> Vec<EventTarget> {
        self.clients
            .values()
            .filter_map(|c| {
                let mask = c.event_masks.get(&window).copied().unwrap_or(0);
                if mask & event_mask != 0 {
                    Some(Self::event_target_for_client(c))
                } else {
                    None
                }
            })
            .collect()
    }

    #[must_use]
    pub fn client_target(&self, client_id: ClientId) -> Option<EventTarget> {
        self.clients
            .get(&client_id.0)
            .map(Self::event_target_for_client)
    }

    #[must_use]
    pub fn selection_owner_target(&self, selection: AtomId) -> Option<(ResourceId, EventTarget)> {
        let owner_window = self.selections.get(&selection)?.0;
        let owner_client = self.resources.window_owner(owner_window)?;
        let target = self.client_target(owner_client)?;
        Some((owner_window, target))
    }

    pub fn drop_window_subscriptions(&mut self, windows: &[ResourceId]) {
        for client in self.clients.values_mut() {
            for w in windows {
                client.event_masks.remove(w);
            }
        }
    }

    pub fn find_passive_grab(
        &self,
        window: ResourceId,
        button: u8,
        state_mask: u16,
    ) -> Option<PassiveButtonGrab> {
        // X11 GrabButton: the grab activates only when "no other
        // buttons ... are logically down" (XGrabButton-1).
        if button >= 1 && self.buttons_down & !(1u16 << (button - 1)) != 0 {
            return None;
        }
        // Xorg CheckDeviceGrabs walks the sprite trace TOP-DOWN (root
        // first): a grab on an ancestor takes precedence over the same
        // grab deeper in the tree (XTS XGrabButton-4), and root-window
        // WM grabs match clicks anywhere.
        let mut chain = vec![window];
        let mut current = window;
        for _ in 0..256 {
            let Some(w) = self.resources.window(current) else {
                break;
            };
            if w.parent == current {
                break;
            }
            chain.push(w.parent);
            current = w.parent;
        }
        for current in chain.into_iter().rev() {
            for grab in &self.button_grabs {
                if grab.grab_window != current {
                    continue;
                }
                let button_match = grab.button == 0 || grab.button == button;
                let mod_match = grab.modifiers == 0x8000 || grab.modifiers == (state_mask & 0x00ff);
                // Xorg ActivatePointerGrab fails when confine_to is
                // not viewable — the grab does not activate
                // (XGrabButton-3).
                let confine_ok = grab.confine_to.0 == 0
                    || self
                        .resources
                        .window(grab.confine_to)
                        .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
                if button_match && mod_match && confine_ok {
                    return Some(grab.clone());
                }
            }
        }
        None
    }

    /// X11 passive `GrabKey` lookup.
    ///
    /// Phase-2 subset: matches grabs whose `grab_window` is the focused
    /// window, an ancestor of it, or root. The descendant-containing-pointer
    /// case is deferred until a real client needs it.
    #[must_use]
    pub fn find_key_grab(
        &self,
        window: ResourceId,
        keycode: u8,
        state_mask: u16,
    ) -> Option<&KeyGrab> {
        // Top-down walk (root first) — Xorg CheckDeviceGrabs checks
        // the focus trace from the root down, so an ancestor's grab
        // takes precedence (see find_passive_grab).
        let mut chain = vec![window];
        let mut current = window;
        for _ in 0..256 {
            let Some(w) = self.resources.window(current) else {
                break;
            };
            if w.parent == current {
                break;
            }
            chain.push(w.parent);
            current = w.parent;
        }
        if !chain.contains(&crate::resources::ROOT_WINDOW) {
            chain.push(crate::resources::ROOT_WINDOW);
        }
        for current in chain.into_iter().rev() {
            for grab in &self.key_grabs {
                if grab.grab_window != current {
                    continue;
                }
                let key_match = grab.keycode == 0 || grab.keycode == keycode;
                let mod_match = grab.modifiers == 0x8000 || grab.modifiers == (state_mask & 0x00ff);
                if key_match && mod_match {
                    return Some(grab);
                }
            }
        }
        None
    }
}

/// Like `emit_window_event` but operates on a pre-snapshotted target list (use when the lock has
/// already been dropped — e.g. after `destroy_window`).
pub fn fanout_event(
    targets: &[EventTarget],
    encode: impl Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
) {
    for target in targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        encode(&mut buf, seq, target.byte_order);
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}

pub fn fanout_raw_event(targets: &[EventTarget], event: &[u8; 32]) {
    for target in targets {
        let seq = target.last_sequence.load(Ordering::Relaxed);
        let mut buf = *event;
        buf[2] = (seq & 0xff) as u8;
        buf[3] = ((seq >> 8) & 0xff) as u8;
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}

#[must_use]
pub(crate) fn xi2_mask_for_client(
    client: &ClientState,
    target: ResourceId,
    fallback: ResourceId,
    device_candidates: &[u16],
) -> u32 {
    for window in [target, fallback] {
        for deviceid in device_candidates {
            if let Some(mask) = client.xi2_masks.get(&(window, *deviceid)) {
                return *mask;
            }
        }
        if fallback == target {
            break;
        }
    }
    0
}

pub fn emit_window_event(
    state: &Mutex<ServerState>,
    window: ResourceId,
    mask_bit: u32,
    encode: impl Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
) {
    let targets = match state.lock() {
        Ok(g) => g.subscribers(window, mask_bit),
        Err(_) => return,
    };
    fanout_event(&targets, encode);
}

#[allow(clippy::too_many_lines)]
pub fn pointer_event_fanout(
    state: &Mutex<ServerState>,
    xid_map: &crate::host_x11::HostXidMap,
    event: crate::host_x11::HostPointerEvent,
) {
    pointer_event_fanout_inner(state, xid_map, event, true);
}

/// Re-routes a thawed ButtonPress as if no passive grab had matched.
/// Called by AllowEvents ReplayPointer. This intentionally does not
/// re-check passive grabs, otherwise the same event would immediately
/// refreeze on the same grab.
pub fn route_button_press_no_grab(
    state: &Mutex<ServerState>,
    xid_map: &crate::host_x11::HostXidMap,
    event: crate::host_x11::HostPointerEvent,
) {
    pointer_event_fanout_inner(state, xid_map, event, false)
}

#[allow(clippy::too_many_lines)]
fn pointer_event_fanout_inner(
    state: &Mutex<ServerState>,
    xid_map: &crate::host_x11::HostXidMap,
    event: crate::host_x11::HostPointerEvent,
    handle_grabs: bool,
) {
    use crate::host_x11::PointerEventKind;
    trace!(
        "pointer_event_fanout: kind={:?} detail={} host_xid=0x{:x} root=({},{}) event=({},{}) state=0x{:x}",
        event.kind,
        event.detail,
        event.host_xid,
        event.root_x,
        event.root_y,
        event.event_x,
        event.event_y,
        event.state
    );

    // Translate root_x/root_y from host-screen coordinates into ynest-root
    // coordinates. The host pump reports root_x/y relative to the host server's
    // root window, but our nested clients see the ynest container as their
    // root, so values must be relative to that. For events on a registered
    // top-level subwindow we have host event_x/y (relative to that subwindow)
    // and the top-level's known position in nested-root, so the translation is
    // straightforward. Without this translation, clients placing popups or
    // tooltips at root_x/root_y end up off-screen by the container's host
    // offset.
    let event = if let Some(top_level_id) = xid_map.get(&event.host_xid).copied() {
        let translated = state.lock().ok().and_then(|g| {
            g.resources
                .window(top_level_id)
                .map(|w| (w.x + event.event_x, w.y + event.event_y))
        });
        if let Some((rx, ry)) = translated {
            crate::host_x11::HostPointerEvent {
                root_x: rx,
                root_y: ry,
                ..event
            }
        } else {
            event
        }
    } else {
        event
    };

    // Active pointer grab: redirect all button/motion events to grab owner.
    // event_x/event_y must be relative to the grab_window (per X11 spec) so
    // the grab owner can locate which child window (menu item, button…)
    // was clicked. Without this translation a WM-popup grab sees clicks at
    // root coordinates and can't match them against its menu-item children.
    let grab_state = if handle_grabs {
        match state.lock() {
            Ok(g) => g.pointer_grab.and_then(|(client_id, grab_window)| {
                let target = g.client_target(client_id)?;
                let (gx, gy) = g.resources.window_absolute_position(grab_window);
                let owner_events = if g.pointer_grab_is_passive {
                    g.button_grabs
                        .iter()
                        .rev()
                        .find(|grab| grab.owner == client_id && grab.grab_window == grab_window)
                        .is_some_and(|grab| grab.owner_events)
                } else {
                    g.active_pointer_grab
                        .filter(|grab| grab.owner == client_id)
                        .is_some_and(|grab| grab.owner_events)
                };
                Some((grab_window, client_id, target, gx, gy, owner_events))
            }),
            Err(_) => return,
        }
    } else {
        None
    };
    if let Some((grab_window, _grab_client, target, grab_x, grab_y, owner_events)) = grab_state {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        let event_x = i32::from(event.root_x)
            .saturating_sub(grab_x)
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let event_y = i32::from(event.root_y)
            .saturating_sub(grab_y)
            .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        let target_within_grab_window = match state.lock() {
            Ok(g) => {
                let hit_window =
                    g.root_pointer_target_at(event.root_x, event.root_y)
                        .or_else(|| {
                            xid_map.get(&event.host_xid).copied().and_then(|top| {
                                g.pointer_target_at(top, event.event_x, event.event_y)
                            })
                        })
                        .map(|(window, _, _)| window);
                hit_window.is_some_and(|w| {
                    w == grab_window || g.resources.is_descendant_of(w, grab_window)
                })
            }
            Err(_) => false,
        };
        let redirect_to_grab = !owner_events || !target_within_grab_window;
        match event.kind {
            PointerEventKind::ButtonPress => x11::encode_button_press_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: grab_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    child: ResourceId(0),
                    state: event.state,
                },
            ),
            PointerEventKind::ButtonRelease => x11::encode_button_release_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: grab_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    child: ResourceId(0),
                    state: event.state,
                },
            ),
            PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: 0,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: grab_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    child: ResourceId(0),
                    state: event.state,
                },
            ),
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => return,
        }
        if redirect_to_grab && let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }

    if event.kind == PointerEventKind::ButtonRelease
        && let Ok(mut s) = state.lock()
        && s.pointer_grab_is_passive
    {
        s.pointer_grab = None;
        s.pointer_grab_is_passive = false;
        s.frozen_pointer_event = None;
        s.frozen_pointer_queue.clear();
    }

    // Passive button grab matching for ButtonPress events.
    if handle_grabs && event.kind == PointerEventKind::ButtonPress {
        let top_level_id_opt = xid_map.get(&event.host_xid).copied();
        let matched = top_level_id_opt.and_then(|top| {
            let s = state.lock().ok()?;
            let (hit_window, _, _) = s
                .root_pointer_target_at(event.root_x, event.root_y)
                .or_else(|| s.pointer_target_at(top, event.event_x, event.event_y))
                .unwrap_or((top, event.event_x, event.event_y));
            s.find_passive_grab(hit_window, event.detail, event.state)
                .map(|grab| (grab, hit_window))
        });
        if let Some(grab) = matched {
            let (grab, hit_window) = grab;
            let target_within_grab_window = match state.lock() {
                Ok(s) => {
                    hit_window == grab.grab_window
                        || s.resources.is_descendant_of(hit_window, grab.grab_window)
                }
                Err(_) => false,
            };
            let redirect_to_grab = !grab.owner_events || !target_within_grab_window;
            let target_opt = match state.lock() {
                Ok(mut s) => {
                    let target = s.client_target(grab.owner);
                    if grab.pointer_mode == 0 {
                        s.frozen_pointer_event = Some(event);
                    }
                    s.pointer_grab = Some((grab.owner, grab.grab_window));
                    s.pointer_grab_is_passive = true;
                    target
                }
                Err(_) => return,
            };
            if redirect_to_grab {
                if let Some(target) = target_opt {
                    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_button_press_event(
                        &mut buf,
                        target.byte_order,
                        x11::PointerEvent {
                            sequence: seq,
                            detail: event.detail,
                            time: event.time,
                            root: crate::resources::ROOT_WINDOW,
                            event: grab.grab_window,
                            root_x: event.root_x,
                            root_y: event.root_y,
                            event_x: event.event_x,
                            event_y: event.event_y,
                            child: ResourceId(0),
                            state: event.state,
                        },
                    );
                    if let Ok(mut w) = target.writer.lock() {
                        let _ = w.write_all(&buf);
                    }
                }
                return;
            }
        }
    }

    let top_level_id = match xid_map.get(&event.host_xid).copied() {
        Some(id) => id,
        None => return,
    };
    let mask_bit: u32 = match event.kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => {
            // PointerMotion (0x40), plus ButtonMotion (0x2000) and the
            // matching ButtonNMotion bit for each currently held button.
            // event.state bits 8..=12 are Button1..Button5.
            let mut bits: u32 = 0x0000_0040;
            let buttons_held = (event.state >> 8) & 0x1f;
            if buttons_held != 0 {
                bits |= 0x0000_2000;
                for n in 0..5 {
                    if buttons_held & (1 << n) != 0 {
                        bits |= 0x0000_0100 << n;
                    }
                }
            }
            bits
        }
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    };
    let xi2_evtype: u16 = match event.kind {
        PointerEventKind::ButtonPress => 4,
        PointerEventKind::ButtonRelease => 5,
        PointerEventKind::MotionNotify => 6,
        PointerEventKind::EnterNotify => 7,
        PointerEventKind::LeaveNotify => 8,
    };
    // XI2 raw events fire alongside the device events when a client has
    // selected XI_Raw* on the root window (xeyes uses RawMotion as a
    // cursor-moved trigger, then calls XIQueryPointer for the position).
    let xi2_raw_evtype: Option<u16> = match event.kind {
        PointerEventKind::ButtonPress => Some(15), // XI_RawButtonPress
        PointerEventKind::ButtonRelease => Some(16), // XI_RawButtonRelease
        PointerEventKind::MotionNotify => Some(17), // XI_RawMotion
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
    };

    let (nested_id, event_x, event_y, core_targets, xi2_targets, xi2_raw_targets) = match state
        .lock()
    {
        Ok(g) => {
            let (target, target_x, target_y) = g
                .root_pointer_target_at(event.root_x, event.root_y)
                .or_else(|| g.pointer_target_at(top_level_id, event.event_x, event.event_y))
                .unwrap_or((top_level_id, event.event_x, event.event_y));

            // Walk up the parent chain to the first window any client is
            // subscribed on. Without this, a click on a window that doesn't
            // select pointer events (e.g. e16's full-screen "Root-bg" cover)
            // never bubbles to root where the WM is listening.
            let (nested_id, event_x, event_y, core_targets) = g
                .pointer_propagation_target(target, target_x, target_y, mask_bit)
                .unwrap_or((target, target_x, target_y, Vec::new()));

            let mut xi2_targets = Vec::new();
            let mut xi2_raw_targets = Vec::new();
            if xi2_evtype != 0 {
                for (cid, c) in g.clients.iter() {
                    let mask = xi2_mask_for_client(c, target, top_level_id, &[4, 2, 1, 0]);
                    trace!(
                        "  xi2 lookup: client={} target=0x{:x} top_level=0x{:x} mask=0x{:x} want_bit={}",
                        cid,
                        target.0,
                        top_level_id.0,
                        mask,
                        1u32 << xi2_evtype
                    );
                    if mask & (1 << xi2_evtype) != 0 {
                        xi2_targets.push(ServerState::event_target_for_client(c));
                    }
                    // XI_Raw* events are typically selected on the root
                    // window; xi2_mask_for_client falls back through
                    // (target, fallback) so a root-window selection on
                    // device 0/1/2 will be found when the cursor is over
                    // any window.
                    if let Some(raw_evtype) = xi2_raw_evtype
                        && mask & (1 << raw_evtype) != 0
                    {
                        xi2_raw_targets.push(ServerState::event_target_for_client(c));
                    }
                    // Also probe the root window for raw events — clients
                    // commonly select XI_Raw* on root with deviceid=1
                    // (XIAllDevices). The lookup above already includes
                    // (target, fallback=top_level); add an explicit root
                    // fallback for raw events specifically.
                    if let Some(raw_evtype) = xi2_raw_evtype {
                        let root_mask = xi2_mask_for_client(
                            c,
                            crate::resources::ROOT_WINDOW,
                            crate::resources::ROOT_WINDOW,
                            &[1, 0, 4, 2],
                        );
                        if root_mask & (1 << raw_evtype) != 0
                            // Avoid double-add if the per-target lookup
                            // also found the same client via the same
                            // selection.
                            && !xi2_raw_targets
                                .iter()
                                .any(|t: &EventTarget| Arc::ptr_eq(&t.writer, &c.writer))
                        {
                            xi2_raw_targets.push(ServerState::event_target_for_client(c));
                        }
                    }
                }
            }

            (
                nested_id,
                event_x,
                event_y,
                core_targets,
                xi2_targets,
                xi2_raw_targets,
            )
        }
        Err(_) => return,
    };

    for target in core_targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        match event.kind {
            PointerEventKind::ButtonPress => x11::encode_button_press_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    child: ResourceId(0),
                    state: event.state,
                },
            ),
            PointerEventKind::ButtonRelease => x11::encode_button_release_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    child: ResourceId(0),
                    state: event.state,
                },
            ),
            PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: 0,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    child: ResourceId(0),
                    state: event.state,
                },
            ),
            PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
                &mut buf,
                target.byte_order,
                x11::CrossingEvent {
                    sequence: seq,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    child: ResourceId(event.child),
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                    detail: event.detail,
                    mode: event.crossing_mode,
                },
            ),
            PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
                &mut buf,
                target.byte_order,
                x11::CrossingEvent {
                    sequence: seq,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    child: ResourceId(event.child),
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                    detail: event.detail,
                    mode: event.crossing_mode,
                },
            ),
        }
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }

    for target in xi2_raw_targets {
        let Some(raw_evtype) = xi2_raw_evtype else {
            break;
        };
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(68);
        x11::encode_xi2_raw_event(
            &mut buf,
            target.byte_order,
            seq,
            137, // XI2 major opcode
            raw_evtype,
            2, // deviceid: Master Pointer
            event.time,
            u32::from(event.detail),
            2, // sourceid: Master Pointer
            i32::from(event.root_x),
            i32::from(event.root_y),
        );
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }

    for target in xi2_targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(84);
        if matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) {
            x11::encode_xi2_crossing_event(
                &mut buf,
                target.byte_order,
                seq,
                137,
                xi2_evtype,
                2,
                event.time,
                crate::resources::ROOT_WINDOW,
                nested_id,
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                0,
                0,
                2,
            );
        } else {
            // Pre-D3 legacy emitter (state.fanout_pointer). Mirror the
            // D3 fanout's XIPointerEmulated handling for scroll-wheel
            // emulation; same rationale as in
            // `core_loop::pointer_fanout`.
            let xi2_flags: u32 = if matches!(
                event.kind,
                crate::host_x11::PointerEventKind::ButtonPress
                    | crate::host_x11::PointerEventKind::ButtonRelease
            ) && (4..=7).contains(&event.detail)
            {
                x11::XI_POINTER_EMULATED
            } else {
                0
            };
            x11::encode_xi2_device_event(
                &mut buf,
                target.byte_order,
                seq,
                137, // XI2 major opcode
                xi2_evtype,
                2, // deviceid: Master Pointer
                event.time,
                crate::resources::ROOT_WINDOW,
                nested_id,
                ResourceId(0), // XI2 doesn't propagate; child=None for hit-target events
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                u32::from(event.detail),
                2,
                xi2_flags,
            );
        }
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}

/// Highest-first evaluation of "given current level + idle time, what
/// should `power_level` become?" — leapfrogs equal timeouts and skips
/// zero-disabled levels. Matches Xorg `os/WaitFor.c:446-448`.
#[must_use]
pub fn next_dpms_level(current: u8, idle_ms: u32, dpms: &DpmsState) -> u8 {
    if current < 3 && dpms.off_ms > 0 && idle_ms >= dpms.off_ms {
        return 3;
    }
    if current < 2 && dpms.suspend_ms > 0 && idle_ms >= dpms.suspend_ms {
        return 2;
    }
    if current < 1 && dpms.standby_ms > 0 && idle_ms >= dpms.standby_ms {
        return 1;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn float_atom_is_pre_interned_at_server_init() {
        let state = ServerState::new();
        assert_ne!(state.float_atom.0, 0, "FLOAT must be interned at startup");
        // Re-interning must hit the cache and return the same id.
        let mut state = state;
        let again = state.atoms.intern("FLOAT", true);
        assert_eq!(again, state.float_atom);
    }

    #[test]
    fn first_client_base_is_above_root_resources() {
        let mut a = IdAllocator::new();
        let (base, mask) = a.allocate().expect("first allocate");
        assert_eq!(base, 0x0010_0000);
        assert_eq!(mask, 0x000F_FFFF);
    }

    #[test]
    fn allocate_increments_by_first_client_base() {
        let mut a = IdAllocator::new();
        let (b1, _) = a.allocate().unwrap();
        let (b2, _) = a.allocate().unwrap();
        assert_eq!(b2 - b1, FIRST_CLIENT_BASE);
    }

    #[test]
    fn release_recycles_base_for_next_allocate() {
        let mut a = IdAllocator::new();
        let (b1, _) = a.allocate().unwrap();
        let (b2, _) = a.allocate().unwrap();
        a.release(b1);
        let (b3, _) = a.allocate().unwrap();
        assert_eq!(
            b3, b1,
            "released base must be reused before bumping next_base"
        );
        let (b4, _) = a.allocate().unwrap();
        assert_eq!(
            b4,
            b2 + FIRST_CLIENT_BASE,
            "fresh base resumes from next_base"
        );
    }

    #[test]
    fn release_ignores_unaligned_or_below_first_base() {
        let mut a = IdAllocator::new();
        let (b1, _) = a.allocate().unwrap();
        a.release(b1 | 0x42); // unaligned (low bits set)
        a.release(0); // below FIRST_CLIENT_BASE
        a.release(0x1234); // below FIRST_CLIENT_BASE
        // Free list rejected all three; next allocate falls through to monotonic.
        let (b2, _) = a.allocate().unwrap();
        assert_eq!(b2, b1 + FIRST_CLIENT_BASE);
    }

    #[test]
    fn release_survives_u32_overflow_threshold() {
        // Drain the monotonic counter to the verge of overflow, then
        // confirm a release-and-reallocate keeps working past the
        // point where a non-recycling allocator would return None.
        let mut a = IdAllocator::new();
        let mut bases = Vec::new();
        while let Some((b, _)) = a.allocate() {
            bases.push(b);
        }
        // u32::MAX / FIRST_CLIENT_BASE = 4095, but `checked_add` rejects the
        // step that *would* land on 4095 * FCB because the *next* base would
        // overflow. Net successful allocates = 4094 (bases 1*FCB through
        // 4094*FCB).
        assert_eq!(
            bases.len(),
            4094,
            "successful monotonic allocates before overflow"
        );
        assert!(
            a.allocate().is_none(),
            "next monotonic allocate must overflow"
        );
        let recycled = bases.pop().unwrap();
        a.release(recycled);
        let (reused, _) = a
            .allocate()
            .expect("recycled base allocates after overflow");
        assert_eq!(reused, recycled);
        assert!(
            a.allocate().is_none(),
            "no more free + monotonic overflowed"
        );
    }

    #[test]
    fn validate_owned_accepts_ids_in_range() {
        let (base, mask) = (0x0020_0000, 0x000F_FFFF);
        assert!(IdAllocator::validate_owned(base, base, mask));
        assert!(IdAllocator::validate_owned(base | mask, base, mask));
        assert!(IdAllocator::validate_owned(base + 0x42, base, mask));
    }

    #[test]
    fn validate_owned_rejects_ids_outside_range() {
        let (base, mask) = (0x0020_0000, 0x000F_FFFF);
        assert!(!IdAllocator::validate_owned(0x0010_0000, base, mask));
        assert!(!IdAllocator::validate_owned(0x0030_0000, base, mask));
        assert!(!IdAllocator::validate_owned(0x0000_0100, base, mask));
    }

    proptest! {
        #[test]
        fn pairwise_non_overlap(n in 1usize..256) {
            let mut a = IdAllocator::new();
            let mut ranges = Vec::with_capacity(n);
            for _ in 0..n {
                ranges.push(a.allocate().expect("range"));
            }
            for (i, (b1, m1)) in ranges.iter().enumerate() {
                for (b2, m2) in ranges.iter().skip(i + 1) {
                    let lo1 = *b1;
                    let hi1 = b1 | m1;
                    let lo2 = *b2;
                    let hi2 = b2 | m2;
                    prop_assert!(hi1 < lo2 || hi2 < lo1, "overlap {:x}..={:x} vs {:x}..={:x}", lo1, hi1, lo2, hi2);
                }
            }
        }

        #[test]
        fn mask_covers_assigned_bits(n in 1usize..64) {
            let mut a = IdAllocator::new();
            for _ in 0..n {
                let (base, mask) = a.allocate().unwrap();
                prop_assert_eq!(base & mask, 0);
            }
        }

        #[test]
        fn allocated_bases_above_root_range(n in 1usize..64) {
            let mut a = IdAllocator::new();
            for _ in 0..n {
                let (base, _) = a.allocate().unwrap();
                prop_assert!(base >= 0x0010_0000);
            }
        }

        #[test]
        fn validate_round_trip(seed in 0u32..256, offset in 0u32..=PER_CLIENT_MASK) {
            let mut a = IdAllocator::new();
            for _ in 0..seed { a.allocate().unwrap(); }
            let (base, mask) = a.allocate().unwrap();
            let id = base + offset;
            prop_assert!(IdAllocator::validate_owned(id, base, mask));
            let other = base.wrapping_add(0x0010_0000).wrapping_add(offset);
            prop_assert!(!IdAllocator::validate_owned(other, base, mask));
        }
    }

    #[test]
    fn subscribers_returns_clients_with_bit_set() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        state.clients.insert(
            2,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        // PropertyChange = 0x0040_0000
        let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn subscribers_omits_other_windows() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x200), 0xFFFF_FFFF)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
        assert!(subs.is_empty());
    }

    #[test]
    fn xi2_pointer_mask_matches_exact_and_wildcard_devices() {
        for deviceid in [2u16, 1, 0] {
            let client = ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::from([((ResourceId(0x100), deviceid), 1 << 4)]),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            };

            assert_eq!(
                xi2_mask_for_client(&client, ResourceId(0x100), ResourceId(0x100), &[2, 1, 0]),
                1 << 4
            );
        }
    }

    #[test]
    fn xi2_keyboard_mask_matches_exact_and_wildcard_devices() {
        for deviceid in [3u16, 1, 0] {
            let client = ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::from([((ResourceId(0x100), deviceid), 1 << 2)]),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            };

            assert_eq!(
                xi2_mask_for_client(&client, ResourceId(0x100), ResourceId(0x100), &[3, 1, 0]),
                1 << 2
            );
        }
    }

    #[test]
    fn subscribers_omits_disconnected_client() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
        state.clients.remove(&1);
        assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
    }

    #[test]
    fn subscribers_intersecting_matches_any_selected_bit() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0b1010)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        state.clients.insert(
            2,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0b0100)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );

        assert_eq!(
            state
                .subscribers_intersecting(ResourceId(0x100), 0b0010)
                .len(),
            1
        );
        assert_eq!(
            state
                .subscribers_intersecting(ResourceId(0x100), 0b1100)
                .len(),
            2
        );
        assert!(
            state
                .subscribers_intersecting(ResourceId(0x100), 0b0001)
                .is_empty()
        );
    }

    #[test]
    fn client_target_returns_connected_client() {
        let mut state = ServerState::new();
        state.clients.insert(
            7,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0x1234)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );

        assert!(state.client_target(ClientId(7)).is_some());
        assert!(state.client_target(ClientId(8)).is_none());
    }

    fn make_test_writer() -> Arc<Mutex<UnixStream>> {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        Arc::new(Mutex::new(a))
    }

    #[test]
    fn unmap_notify_fanout_reaches_only_subscribed_clients() {
        use yserver_protocol::x11::{SequenceNumber, encode_unmap_notify_event};

        // Client A: StructureNotify on window 0x100.
        let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client B: KeyPress only on window 0x100 (NOT StructureNotify).
        let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");

        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(a_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0002_0000)]), // StructureNotify
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        state.clients.insert(
            2,
            ClientState {
                writer: Arc::new(Mutex::new(b_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]), // KeyPress
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );

        let subs = state.subscribers(ResourceId(0x100), 0x0002_0000);
        assert_eq!(subs.len(), 1, "only client A should be subscribed");

        let target = &subs[0];
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        encode_unmap_notify_event(
            &mut buf,
            seq,
            target.byte_order,
            ResourceId(0x100),
            ResourceId(0x100),
            false,
        );
        assert_eq!(buf[0], 18, "wire byte 0 is UnmapNotify");
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
        assert_eq!(buf[12], 0, "from_configure = false");
    }

    #[test]
    fn drop_window_subscriptions_removes_entries_for_destroyed_windows() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([
                    (ResourceId(0x100), 0x0040_0000),
                    (ResourceId(0x200), 0x0040_0000),
                ]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
        state.drop_window_subscriptions(&[ResourceId(0x100)]);
        assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
        // Surviving window's subscription stays.
        assert_eq!(state.subscribers(ResourceId(0x200), 0x0040_0000).len(), 1);
    }

    #[test]
    fn replay_pointer_delivers_to_button_press_window_not_grab_owner() {
        use std::{
            collections::HashMap as StdHashMap,
            io::{ErrorKind, Read},
            sync::Mutex as StdMutex,
        };

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        let (grab_writer_local, mut grab_reader_remote) = UnixStream::pair().expect("socketpair");
        let (target_writer_local, mut target_reader_remote) =
            UnixStream::pair().expect("socketpair");
        grab_reader_remote.set_nonblocking(true).unwrap();
        target_reader_remote.set_nonblocking(true).unwrap();

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            let grab_window = ResourceId(0x0010_0002);
            let target_window = ResourceId(0x0020_0002);
            s.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: grab_window,
                    parent: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            s.resources.create_window(
                ClientId(2),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: target_window,
                    parent: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = s.resources.map_window(grab_window);
            let _ = s.resources.map_window(target_window);
            s.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(grab_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(grab_window, 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.clients.insert(
                2,
                ClientState {
                    writer: Arc::new(Mutex::new(target_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(target_window, 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.pointer_grab = Some((ClientId(1), grab_window));
            s.pointer_grab_is_passive = true;
            assert_eq!(s.subscribers(grab_window, 0x0000_0004).len(), 1);
            assert_eq!(s.subscribers(target_window, 0x0000_0004).len(), 1);
            assert!(s.resources.window(target_window).is_some());
            assert!(
                s.resources
                    .pointer_target_at(target_window, 10, 10)
                    .is_some()
            );
            assert!(
                s.pointer_propagation_target(target_window, 10, 10, 0x0000_0004)
                    .is_some()
            );
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0020_0002));
        let xid_map = map;

        route_button_press_no_grab(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE,
                detail: 1,
                time: 0,
                root_x: 10,
                root_y: 10,
                event_x: 10,
                event_y: 10,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
        );

        let mut buf = [0u8; 32];
        let grab_read = grab_reader_remote.read(&mut buf);
        assert!(
            matches!(grab_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "grab owner must not receive replayed ButtonPress; got {grab_read:?}",
        );
        let target_read = target_reader_remote.read(&mut buf);
        assert!(
            matches!(target_read, Ok(32)),
            "target window subscriber should receive replayed ButtonPress; got {target_read:?}",
        );
        assert_eq!(buf[0], 4, "event type should be ButtonPress");
        assert_eq!(&buf[12..16], &0x0020_0002u32.to_le_bytes());
    }

    #[test]
    fn passive_grab_owner_events_keeps_child_delivery_on_owned_windows() {
        use std::{
            collections::HashMap as StdHashMap,
            io::{ErrorKind, Read},
            sync::Mutex as StdMutex,
        };

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        let (grab_writer_local, mut grab_reader_remote) = UnixStream::pair().expect("socketpair");
        let (child_writer_local, mut child_reader_remote) = UnixStream::pair().expect("socketpair");
        grab_reader_remote.set_nonblocking(true).unwrap();
        child_reader_remote.set_nonblocking(true).unwrap();

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            let grab_window = ResourceId(0x0010_0002);
            let child_window = ResourceId(0x0010_0003);
            s.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: grab_window,
                    parent: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            s.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: child_window,
                    parent: grab_window,
                    x: 10,
                    y: 10,
                    width: 40,
                    height: 40,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = s.resources.map_window(grab_window);
            let _ = s.resources.map_window(child_window);
            s.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(grab_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(grab_window, 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.clients.insert(
                2,
                ClientState {
                    writer: Arc::new(Mutex::new(child_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(child_window, 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.pointer_grab = Some((ClientId(1), grab_window));
            s.pointer_grab_is_passive = true;
            s.button_grabs.push(PassiveButtonGrab {
                owner: ClientId(1),
                grab_window,
                button: 1,
                modifiers: 0,
                owner_events: true,
                event_mask: 0xFFFF_FFFF,
                pointer_mode: 0,
                keyboard_mode: 1,
                confine_to: ResourceId(0),
                via_xi2: true,
            });
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
        let xid_map = map;

        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE,
                detail: 1,
                time: 0,
                root_x: 20,
                root_y: 20,
                event_x: 20,
                event_y: 20,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
        );

        let mut buf = [0u8; 32];
        let child_read = child_reader_remote.read(&mut buf);
        assert!(
            matches!(child_read, Ok(32)),
            "owner_events=true passive grab must still deliver to the owned child; got {child_read:?}",
        );
        assert_eq!(buf[0], 4, "event type should be ButtonPress");
        assert_eq!(&buf[12..16], &0x0010_0003u32.to_le_bytes());

        let grab_read = grab_reader_remote.read(&mut buf);
        assert!(
            matches!(grab_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "owner_events=true passive grab must not redirect owned-child clicks to the grab owner; got {grab_read:?}",
        );
    }

    #[test]
    fn passive_grab_owner_events_keeps_descendant_delivery_even_when_child_owned_elsewhere() {
        use std::{
            collections::HashMap as StdHashMap,
            io::{ErrorKind, Read},
            sync::Mutex as StdMutex,
        };

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        let (grab_writer_local, mut grab_reader_remote) = UnixStream::pair().expect("socketpair");
        let (child_writer_local, mut child_reader_remote) = UnixStream::pair().expect("socketpair");
        grab_reader_remote.set_nonblocking(true).unwrap();
        child_reader_remote.set_nonblocking(true).unwrap();

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            let grab_window = ResourceId(0x0010_0010);
            let child_window = ResourceId(0x0010_0011);
            s.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: grab_window,
                    parent: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            s.resources.create_window(
                ClientId(2),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: child_window,
                    parent: grab_window,
                    x: 10,
                    y: 10,
                    width: 40,
                    height: 40,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = s.resources.map_window(grab_window);
            let _ = s.resources.map_window(child_window);
            s.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(grab_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(grab_window, 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.clients.insert(
                2,
                ClientState {
                    writer: Arc::new(Mutex::new(child_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(child_window, 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.pointer_grab = Some((ClientId(1), grab_window));
            s.pointer_grab_is_passive = true;
            s.button_grabs.push(PassiveButtonGrab {
                owner: ClientId(1),
                grab_window,
                button: 1,
                modifiers: 0,
                owner_events: true,
                event_mask: 0xFFFF_FFFF,
                pointer_mode: 0,
                keyboard_mode: 1,
                confine_to: ResourceId(0),
                via_xi2: true,
            });
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0010_0010));
        let xid_map = map;

        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE,
                detail: 1,
                time: 0,
                root_x: 20,
                root_y: 20,
                event_x: 20,
                event_y: 20,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
        );

        let mut buf = [0u8; 32];
        let child_read = child_reader_remote.read(&mut buf);
        assert!(
            matches!(child_read, Ok(32)),
            "owner_events=true passive grab must still deliver to the descendant child even when another client owns it; got {child_read:?}",
        );
        let grab_read = grab_reader_remote.read(&mut buf);
        assert!(
            matches!(grab_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "owner_events=true passive grab must not redirect descendant clicks to the grab owner; got {grab_read:?}",
        );
    }

    #[test]
    fn pointer_event_fanout_filters_by_mask() {
        use std::{collections::HashMap as StdHashMap, sync::Mutex as StdMutex};

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        // Client A: ButtonPress on window 0x0010_0002.
        let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client B: MotionNotify on window 0x0010_0002.
        let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client C: no pointer events at all.
        let (c_writer_local, _c_reader_remote) = UnixStream::pair().expect("socketpair");

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            s.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(a_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0004)]), // ButtonPress
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.clients.insert(
                2,
                ClientState {
                    writer: Arc::new(Mutex::new(b_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0040)]), // PointerMotion
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.clients.insert(
                3,
                ClientState {
                    writer: Arc::new(Mutex::new(c_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0030_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::new(),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
        let xid_map = map;

        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE,
                detail: 1,
                time: 0,
                root_x: 1,
                root_y: 2,
                event_x: 3,
                event_y: 4,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
        );

        let s = state.lock().unwrap();
        assert_eq!(
            s.subscribers(ResourceId(0x0010_0002), 0x0000_0004).len(),
            1,
            "only client A selected ButtonPress"
        );
        assert_eq!(
            s.subscribers(ResourceId(0x0010_0002), 0x0000_0040).len(),
            1,
            "only client B selected MotionNotify"
        );
    }

    #[test]
    fn pointer_event_fanout_delivers_motion_under_button_motion_mask() {
        use std::{
            collections::HashMap as StdHashMap,
            io::{ErrorKind, Read},
            sync::Mutex as StdMutex,
        };

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        // Client A: subscribes to ButtonMotion (0x2000) only. Mirrors
        // wmaker's frame mask: it expects motion while a button is held.
        let (a_writer_local, mut a_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client B: no motion mask at all — must not receive anything.
        let (b_writer_local, mut b_reader_remote) = UnixStream::pair().expect("socketpair");

        a_reader_remote.set_nonblocking(true).unwrap();
        b_reader_remote.set_nonblocking(true).unwrap();

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            // Top-level window so pointer_target_at returns the same id.
            s.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: ResourceId(0x0010_0002),
                    parent: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            let _ = s.resources.map_window(ResourceId(0x0010_0002));
            s.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(a_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_2000)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            s.clients.insert(
                2,
                ClientState {
                    writer: Arc::new(Mutex::new(b_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::new(),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
        let xid_map = map;

        // Motion with button 1 held (state bit 8 == 0x100).
        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::MotionNotify,
                host_xid: 0xCAFE,
                detail: 0,
                time: 0,
                root_x: 5,
                root_y: 5,
                event_x: 5,
                event_y: 5,
                state: 0x0100,
                crossing_mode: 0,
                child: 0,
            },
        );

        let mut buf = [0u8; 32];
        let a_read = a_reader_remote.read(&mut buf);
        assert!(
            matches!(a_read, Ok(32)),
            "client with ButtonMotion mask should receive 32-byte MotionNotify when a button is held; got {a_read:?}",
        );
        assert_eq!(buf[0], 6, "event type should be MotionNotify");

        let b_read = b_reader_remote.read(&mut buf);
        assert!(
            matches!(b_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "client with no motion mask must not receive motion; got {b_read:?}",
        );

        // Motion without any button held: ButtonMotion subscriber must NOT receive.
        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::MotionNotify,
                host_xid: 0xCAFE,
                detail: 0,
                time: 0,
                root_x: 5,
                root_y: 5,
                event_x: 5,
                event_y: 5,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
        );
        let a_read2 = a_reader_remote.read(&mut buf);
        assert!(
            matches!(a_read2, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "ButtonMotion-only subscriber must NOT receive motion when no button is held; got {a_read2:?}",
        );
    }

    #[test]
    fn pointer_event_fanout_drops_unknown_host_xid() {
        use std::{collections::HashMap as StdHashMap, sync::Mutex as StdMutex};

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            s.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(a_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
        }

        let xid_map: crate::host_x11::HostXidMap = StdHashMap::new(); // empty

        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE, // not in map
                detail: 1,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
        );

        assert!(state.lock().unwrap().clients.contains_key(&1));
    }

    #[test]
    fn key_grab_lookup_exact_match() {
        let mut s = ServerState::new();
        let win = ResourceId(0x42);
        let owner = ClientId(1);
        s.key_grabs.push(KeyGrab {
            owner,
            grab_window: win,
            keycode: 24,
            modifiers: 0x0040,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
            via_xi2: false,
        });
        let hit = s.find_key_grab(win, 24, 0x0040);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().owner, owner);
    }

    #[test]
    fn key_grab_lookup_any_modifier_wildcard() {
        let mut s = ServerState::new();
        let win = ResourceId(0x42);
        s.key_grabs.push(KeyGrab {
            owner: ClientId(1),
            grab_window: win,
            keycode: 24,
            modifiers: 0x8000,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
            via_xi2: false,
        });
        assert!(s.find_key_grab(win, 24, 0x0040).is_some());
        assert!(s.find_key_grab(win, 24, 0x0000).is_some());
        assert!(s.find_key_grab(win, 25, 0x0040).is_none());
    }

    #[test]
    fn key_grab_lookup_any_keycode_wildcard() {
        let mut s = ServerState::new();
        let win = ResourceId(0x42);
        s.key_grabs.push(KeyGrab {
            owner: ClientId(1),
            grab_window: win,
            keycode: 0,
            modifiers: 0x0040,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
            via_xi2: false,
        });
        assert!(s.find_key_grab(win, 24, 0x0040).is_some());
        assert!(s.find_key_grab(win, 99, 0x0040).is_some());
        assert!(s.find_key_grab(win, 24, 0x0000).is_none());
    }

    #[test]
    fn active_keyboard_grab_set_and_clear() {
        let mut s = ServerState::new();
        assert!(s.active_keyboard_grab.is_none());
        s.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner: ClientId(7),
            grab_window: ResourceId(0xff),
            source: ActiveKeyboardGrabSource::Explicit,
            owner_events: false,
            via_xi2: false,
        });
        assert_eq!(s.active_keyboard_grab.unwrap().owner, ClientId(7));
        s.active_keyboard_grab = None;
        assert!(s.active_keyboard_grab.is_none());
    }

    fn add_test_client(state: &mut ServerState, client_id: u32, base: u32) {
        state.clients.insert(
            client_id,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: base,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
    }

    #[test]
    fn pointer_propagation_walks_parent_chain_to_root() {
        // Reproduces the desk-1 right-click bug: pointer-on-child of root,
        // child has no ButtonPress mask, root does. The event must propagate
        // up to root.
        use crate::resources::ROOT_WINDOW;
        use yserver_protocol::x11::CreateWindowRequest;

        let mut state = ServerState::new();
        add_test_client(&mut state, 1, 0x0010_0000);

        // Child of root, full screen, no ButtonPress mask (e16's "Root-bg").
        let child = ResourceId(0x0010_0004);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: child,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(child);

        // e16 selects ButtonPress on root.
        let button_press_mask: u32 = 0x0000_0004;
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(ROOT_WINDOW, button_press_mask);

        // Click hits the child at (136, 111) — relative to child since child is
        // at (0, 0). pointer_propagation_target should walk up to root.
        let result = state.pointer_propagation_target(child, 136, 111, button_press_mask);
        assert!(result.is_some(), "expected propagation to root");
        let (window, x, y, subs) = result.unwrap();
        assert_eq!(window, ROOT_WINDOW);
        // Child is at (0, 0) on root, so coords are unchanged.
        assert_eq!(x, 136);
        assert_eq!(y, 111);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn pointer_propagation_translates_offset_coords() {
        // Click at (10, 20) inside a child positioned at (50, 60) on root —
        // should translate to (60, 80) when delivered to root.
        use crate::resources::ROOT_WINDOW;
        use yserver_protocol::x11::CreateWindowRequest;

        let mut state = ServerState::new();
        add_test_client(&mut state, 1, 0x0010_0000);

        let child = ResourceId(0x0010_0010);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: child,
                parent: ROOT_WINDOW,
                x: 50,
                y: 60,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(child);

        let button_press_mask: u32 = 0x0000_0004;
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(ROOT_WINDOW, button_press_mask);

        let (window, x, y, _) = state
            .pointer_propagation_target(child, 10, 20, button_press_mask)
            .expect("propagation should find root");
        assert_eq!(window, ROOT_WINDOW);
        assert_eq!(x, 60);
        assert_eq!(y, 80);
    }

    #[test]
    fn pointer_propagation_stops_at_first_subscriber() {
        // Both child and root subscribe; event delivered to child (first hit).
        use crate::resources::ROOT_WINDOW;
        use yserver_protocol::x11::CreateWindowRequest;

        let mut state = ServerState::new();
        add_test_client(&mut state, 1, 0x0010_0000);

        let child = ResourceId(0x0010_0020);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: child,
                parent: ROOT_WINDOW,
                x: 5,
                y: 5,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(child);

        let button_press_mask: u32 = 0x0000_0004;
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(child, button_press_mask);
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(ROOT_WINDOW, button_press_mask);

        let (window, x, y, _) = state
            .pointer_propagation_target(child, 30, 40, button_press_mask)
            .expect("propagation should hit child first");
        assert_eq!(window, child);
        assert_eq!(x, 30);
        assert_eq!(y, 40);
    }

    #[test]
    fn pointer_propagation_returns_none_when_nothing_subscribes() {
        use crate::resources::ROOT_WINDOW;
        use yserver_protocol::x11::CreateWindowRequest;

        let mut state = ServerState::new();
        add_test_client(&mut state, 1, 0x0010_0000);

        let child = ResourceId(0x0010_0030);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: child,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(child);

        let button_press_mask: u32 = 0x0000_0004;
        let result = state.pointer_propagation_target(child, 10, 10, button_press_mask);
        assert!(result.is_none());
    }

    #[test]
    fn cow_with_empty_input_shape_passes_clicks_to_sibling_below() {
        use crate::resources::{ROOT_VISUAL, ROOT_WINDOW};
        use yserver_protocol::x11::CreateWindowRequest;

        let mut state = ServerState::new();

        // Non-COW sibling at (0,0) 800x600, default (full) input shape.
        let sib = ResourceId(0x0010_0080);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: sib,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(sib);

        // Materialize COW (full-screen, empty input shape per Task 2.8).
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(host_xid);
        state.materialize_cow_input_shape();

        // Click at (50, 50): inside both sibling and COW geometry. COW's
        // empty input shape → hit_test_child(COW) = None → iteration
        // falls through to `sib`.
        let (target, _, _) = state
            .root_pointer_target_at(50, 50)
            .expect("trace hits sibling below COW");
        assert_eq!(
            target, sib,
            "empty COW input shape must let clicks through to sibling below"
        );
    }

    #[test]
    fn cow_with_non_empty_input_shape_descends_into_stage() {
        use crate::resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_VISUAL};
        use yserver_protocol::x11::{CreateWindowRequest, xfixes};

        let mut state = ServerState::new();

        let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(host_xid);
        // Compositor populates COW input shape covering the stage region.
        state
            .shape_windows
            .entry(COMPOSITE_OVERLAY_WINDOW)
            .or_default()
            .input = Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let stage = ResourceId(0x0010_0050);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: stage,
                parent: COMPOSITE_OVERLAY_WINDOW,
                x: 10,
                y: 10,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(stage);

        let (target, _, _) = state.root_pointer_target_at(50, 50).expect("hit");
        assert_eq!(
            target, stage,
            "non-empty COW input shape lets the trace descend to stage"
        );
    }

    #[test]
    fn cow_default_input_shape_is_empty() {
        use crate::resources::COMPOSITE_OVERLAY_WINDOW;

        let mut state = ServerState::new();
        let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(host_xid);
        state.materialize_cow_input_shape();

        let shape = state
            .shape_windows
            .get(&COMPOSITE_OVERLAY_WINDOW)
            .expect("COW must have a shape_windows entry after materialization");
        assert!(
            shape.input.is_some(),
            "COW must have a non-default input shape (set, but empty)"
        );
        assert_eq!(
            shape.input.as_ref().unwrap().len(),
            0,
            "COW's default input shape rects are empty (click-through)"
        );
    }

    #[test]
    fn dpms_transition_deadline_picks_smallest_non_zero_above_current() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        let baseline = Instant::now();
        state.dpms.last_activity = baseline;
        state.dpms.standby_ms = 300_000;
        state.dpms.suspend_ms = 600_000;
        state.dpms.off_ms = 900_000;

        state.dpms.power_level = 0; // On
        assert_eq!(
            state.dpms_transition_deadline(),
            Some(baseline + Duration::from_millis(300_000))
        );

        state.dpms.power_level = 1; // Standby
        assert_eq!(
            state.dpms_transition_deadline(),
            Some(baseline + Duration::from_millis(600_000))
        );

        state.dpms.power_level = 2; // Suspend
        assert_eq!(
            state.dpms_transition_deadline(),
            Some(baseline + Duration::from_millis(900_000))
        );

        state.dpms.power_level = 3; // Off — nothing above
        assert_eq!(state.dpms_transition_deadline(), None);
    }

    #[test]
    fn dpms_transition_deadline_returns_none_when_disabled() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = false; // disabled
        state.dpms.standby_ms = 300_000;
        assert!(state.dpms_transition_deadline().is_none());
    }

    #[test]
    fn dpms_transition_deadline_returns_none_when_not_kms_capable() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = false;
        state.dpms.enabled = true; // a ynest client called DPMSEnable
        state.dpms.standby_ms = 300_000;
        // No backend to drive — no deadline.
        assert!(state.dpms_transition_deadline().is_none());
    }

    #[test]
    fn dpms_transition_deadline_zero_skips_not_halts() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        let baseline = Instant::now();
        state.dpms.last_activity = baseline;
        // Standby + Off disabled, Suspend at 900s.
        state.dpms.standby_ms = 0;
        state.dpms.suspend_ms = 900_000;
        state.dpms.off_ms = 0;

        state.dpms.power_level = 0; // On
        assert_eq!(
            state.dpms_transition_deadline(),
            Some(baseline + Duration::from_millis(900_000))
        );

        state.dpms.power_level = 2; // Suspend — nothing above non-zero
        assert!(state.dpms_transition_deadline().is_none());
    }

    #[test]
    fn next_dpms_level_leapfrogs_on_equal_timeouts() {
        let mut state = ServerState::new();
        state.dpms.standby_ms = 600_000;
        state.dpms.suspend_ms = 600_000;
        state.dpms.off_ms = 600_000;
        // From On, with idle = exactly 600_000ms, highest expired wins → Off.
        assert_eq!(next_dpms_level(0, 600_000, &state.dpms), 3);
    }

    #[test]
    fn next_dpms_level_skips_zero_levels() {
        let mut state = ServerState::new();
        state.dpms.standby_ms = 0;
        state.dpms.suspend_ms = 900_000;
        state.dpms.off_ms = 0;
        // From On, idle = 900s → Suspend (Standby and Off skipped).
        assert_eq!(next_dpms_level(0, 900_000, &state.dpms), 2);
    }

    #[test]
    fn next_dpms_level_stable_when_under_threshold() {
        let mut state = ServerState::new();
        state.dpms.standby_ms = 300_000;
        state.dpms.suspend_ms = 600_000;
        state.dpms.off_ms = 900_000;
        assert_eq!(next_dpms_level(0, 0, &state.dpms), 0);
        assert_eq!(next_dpms_level(1, 100_000, &state.dpms), 1);
        // Already at Off (max level): cascade has nowhere to go.
        assert_eq!(next_dpms_level(3, 999_999_999, &state.dpms), 3);
    }

    #[test]
    fn screensaver_idle_deadline_none_when_timeout_zero() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 0;
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_none_when_suspended() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.screensaver.suspend_counts.insert(ClientId(7), 1);
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn dpms_transition_deadline_none_when_screensaver_suspended() {
        // Xorg WaitFor.c:519 — one timer drives BOTH SS and DPMS, and
        // it isn't armed when screenSaverSuspended. XScreenSaverSuspend
        // therefore inhibits DPMS firing, which mpv/Firefox rely on.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.standby_ms = 300_000;
        state.screensaver.suspend_counts.insert(ClientId(99), 1);
        assert!(state.dpms_transition_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_none_when_active() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.screensaver.active = ScreenSaverActive::On;
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_none_when_dpms_blanked() {
        // Xorg WaitFor.c:457 — when DPMS already blanked the panel
        // the SS idle timer is suppressed (DPMS→SS coupling will
        // have already activated SS on the DPMS transition).
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.dpms.power_level = 1;
        assert!(state.screensaver_idle_deadline().is_none());
    }

    #[test]
    fn screensaver_idle_deadline_returns_last_activity_plus_timeout() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        let baseline = Instant::now();
        state.dpms.last_activity = baseline;
        state.screensaver.timeout_ms = 60_000;
        assert_eq!(
            state.screensaver_idle_deadline(),
            Some(baseline + Duration::from_millis(60_000))
        );
    }

    #[test]
    fn screensaver_cycle_deadline_none_when_off() {
        let state = ServerState::new();
        assert!(state.screensaver_cycle_deadline().is_none());
    }

    #[test]
    fn screensaver_cycle_deadline_some_when_on() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.interval_ms = 600_000;
        let fire_at = Instant::now() + Duration::from_millis(600_000);
        state.screensaver.next_cycle = Some(fire_at);
        assert_eq!(state.screensaver_cycle_deadline(), Some(fire_at));
    }

    #[test]
    fn screensaver_cycle_deadline_propagates_next_cycle_none() {
        // The invariant "interval_ms == 0 ⇒ next_cycle is None" lives
        // in the activation transition (Task 3); here we only verify
        // the deadline helper propagates a None `next_cycle` through.
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.interval_ms = 0;
        state.screensaver.next_cycle = None;
        assert!(state.screensaver_cycle_deadline().is_none());
    }

    #[test]
    fn idletime_baseline_global_returns_dpms_last_activity() {
        use std::time::Instant;
        let mut state = ServerState::new();
        let baseline = Instant::now();
        state.dpms.last_activity = baseline;
        assert_eq!(
            state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_COUNTER),
            baseline
        );
    }

    #[test]
    fn idletime_baseline_per_device_uses_per_device_entry() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        let global = Instant::now() - Duration::from_secs(60);
        let pointer = Instant::now() - Duration::from_secs(5);
        state.dpms.last_activity = global;
        state.per_device_last_activity.insert(2, pointer);
        assert_eq!(
            state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_DEVICE_VCP),
            pointer
        );
        // VCK has no per-device entry; falls back to global.
        assert_eq!(
            state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_DEVICE_VCK),
            global
        );
    }

    #[test]
    fn idletime_baseline_unknown_counter_falls_back_to_global() {
        let state = ServerState::new();
        let baseline = state.dpms.last_activity;
        assert_eq!(state.idletime_baseline(0xdead_beef), baseline);
    }

    #[test]
    fn idletime_alarm_deadline_none_when_no_alarms() {
        let state = ServerState::new();
        assert!(state.idletime_alarm_deadline().is_none());
    }

    #[test]
    fn idletime_alarm_deadline_picks_smallest_active_pos_alarm() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let baseline = std::time::Instant::now();
        state.dpms.last_activity = baseline;

        for (id, wait) in &[(1u32, 60_000i64), (2, 30_000), (3, 90_000)] {
            state.sync_alarms.insert(
                *id,
                crate::server::SyncAlarm {
                    owner: ClientId(1),
                    counter: x11sync::IDLETIME_COUNTER,
                    wait_value: *wait,
                    delta: 0,
                    test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
                    events: true,
                    state: x11sync::ALARM_STATE_ACTIVE,
                },
            );
        }

        let deadline = state.idletime_alarm_deadline().expect("Some");
        let expected = baseline + Duration::from_millis(30_000);
        // Allow ±1ms for monotonic-clock resolution.
        let diff = if deadline > expected {
            deadline - expected
        } else {
            expected - deadline
        };
        assert!(
            diff < Duration::from_millis(2),
            "deadline ~ baseline + 30_000ms; got diff {diff:?}"
        );
    }

    #[test]
    fn idletime_alarm_deadline_ignores_negative_alarms() {
        // Negative-* alarms only fire on input wake, not on a positive
        // deadline. They must not be considered when computing the
        // poll-deadline `.min()`.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.sync_alarms.insert(
            1,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: true,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        assert!(state.idletime_alarm_deadline().is_none());
    }

    #[test]
    fn idletime_alarm_deadline_ignores_inactive_alarms() {
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.sync_alarms.insert(
            1,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
                events: true,
                state: x11sync::ALARM_STATE_INACTIVE,
            },
        );
        assert!(state.idletime_alarm_deadline().is_none());
    }

    #[test]
    fn idletime_alarm_deadline_ignores_quiescent_alarm_whose_threshold_already_passed() {
        // Regression for the quiescent-state skip: a PositiveTransition +
        // delta=0 alarm that has already fired stays Active but is
        // quiescent — it doesn't re-fire until the counter drops below
        // wait_value and crosses back up (which requires input). Such an
        // alarm must NOT contribute a past-instant to the poll-deadline
        // (which would spin the poll loop with Duration::ZERO).
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        // Already idle for 90s; alarm threshold is 60s — quiescent.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state.sync_alarms.insert(
            1,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
                events: true,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        assert!(
            state.idletime_alarm_deadline().is_none(),
            "quiescent alarm (current_idle >= wait_value) must not contribute a deadline"
        );
    }

    #[test]
    fn idletime_alarm_deadline_none_when_screensaver_suspended() {
        // Mirrors the dpms_transition_deadline suspend gate. XScreen-
        // SaverSuspend inhibits both the DPMS cascade AND IDLETIME
        // alarms so fullscreen video (Firefox / mpv / vlc) doesn't
        // blank the screen.
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        state.screensaver.suspend_counts.insert(ClientId(99), 1);
        state.sync_alarms.insert(
            1,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
                events: true,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        assert!(state.idletime_alarm_deadline().is_none());
    }

    /// XC-MISC guard test: every client-XID namespace must register in
    /// xid_occupied. Seeds ONE resource per namespace at a distinct id.
    /// A future XID-keyed map added without xid_occupied coverage should
    /// be caught by review against this pattern (spec
    /// 2026-06-12-xcmisc-design.md "maintenance hazard").
    #[test]
    fn xid_occupied_covers_every_namespace() {
        use crate::{
            backend::{GlyphSetHandle, PictureHandle},
            resources::{GlyphSetState, PictureKind, PictureState, ROOT_VISUAL},
        };
        use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

        let mut state = ServerState::new();
        let owner = ClientId(1);
        let base = 0x0010_0000u32;
        let mut expect = Vec::new();

        // ── 8 ResourceTable namespaces ──

        // 1. window
        let id_window = base + 1;
        state.resources.create_window(
            owner,
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id_window),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        expect.push(id_window);

        // 2. pixmap
        let id_pixmap = base + 2;
        state.resources.create_pixmap(
            owner,
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(id_pixmap),
                drawable: ROOT_WINDOW,
                width: 1,
                height: 1,
            },
        );
        expect.push(id_pixmap);

        // 3. gc
        let id_gc = base + 3;
        state.resources.seed_gc_for_test(owner, ResourceId(id_gc));
        expect.push(id_gc);

        // 4. font
        let id_font = base + 4;
        state
            .resources
            .seed_font_for_test(owner, ResourceId(id_font));
        expect.push(id_font);

        // 5. cursor
        let id_cursor = base + 5;
        state.resources.create_cursor(owner, ResourceId(id_cursor));
        expect.push(id_cursor);

        // 6. colormap
        let id_colormap = base + 6;
        state
            .resources
            .create_colormap(owner, ResourceId(id_colormap), ROOT_VISUAL);
        expect.push(id_colormap);

        // 7. picture (pub map — insert literal directly)
        let id_picture = base + 7;
        state.resources.pictures.insert(
            id_picture,
            PictureState {
                client: owner,
                host_picture_xid: PictureHandle::from_raw_for_test(1),
                host_owned_pixmap: None,
                kind: PictureKind::Sourceless,
                drawable: None,
            },
        );
        expect.push(id_picture);

        // 8. glyphset (pub map — insert literal directly)
        let id_glyphset = base + 8;
        state.resources.glyphsets.insert(
            id_glyphset,
            GlyphSetState {
                client: owner,
                host_glyphset_xid: GlyphSetHandle::from_raw_for_test(1),
            },
        );
        expect.push(id_glyphset);

        // ── 9 ServerState extension namespaces ──

        // 9. xfixes_regions
        let id_xfixes = base + 9;
        state.xfixes_regions.insert(
            id_xfixes,
            XFixesRegion {
                owner,
                rects: vec![],
            },
        );
        expect.push(id_xfixes);

        // 10. sync_counters
        let id_sync_counter = base + 10;
        state
            .sync_counters
            .insert(id_sync_counter, SyncCounter { owner, value: 0 });
        expect.push(id_sync_counter);

        // 11. sync_alarms
        let id_sync_alarm = base + 11;
        state.sync_alarms.insert(
            id_sync_alarm,
            SyncAlarm {
                owner,
                ..SyncAlarm::default()
            },
        );
        expect.push(id_sync_alarm);

        // 12. sync_fences
        let id_sync_fence = base + 12;
        state.sync_fences.insert(
            id_sync_fence,
            SyncFence {
                owner,
                triggered: false,
            },
        );
        expect.push(id_sync_fence);

        // 13. damage_objects
        let id_damage = base + 13;
        state.damage_objects.insert(
            id_damage,
            DamageObject {
                owner,
                drawable: ROOT_WINDOW,
                level: 0,
                rects: vec![],
                pending_notify_fired: false,
                last_reported_geometry: None,
            },
        );
        expect.push(id_damage);

        // 14. mit_shm_segments — requires a real fd; use memfd_create
        let id_shm = base + 14;
        let fd = unsafe { libc::memfd_create(c"xcmisc-test-shm".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let rc = unsafe { libc::ftruncate(fd, 4096) };
        assert_eq!(rc, 0, "ftruncate failed");
        let shm_seg = MitShmSegment::from_fd(owner, fd, false).expect("MitShmSegment::from_fd");
        state.mit_shm_segments.insert(id_shm, shm_seg);
        expect.push(id_shm);

        // 15. glx_contexts
        let id_glx_ctx = base + 15;
        state.glx_contexts.insert(
            id_glx_ctx,
            GlxContext {
                owner,
                fbconfig: 0,
                render_type: 0,
            },
        );
        expect.push(id_glx_ctx);

        // 16. glx_drawables
        let id_glx_draw = base + 16;
        state.glx_drawables.insert(
            id_glx_draw,
            GlxDrawable {
                owner,
                x_drawable: 0,
                fbconfig: 0,
                width: 0,
                height: 0,
                attributes: vec![],
                glx_export_host_xid: None,
            },
        );
        expect.push(id_glx_draw);

        // 17. present_event_selections
        let id_present = base + 17;
        state.present_event_selections.insert(
            id_present,
            PresentEventSelection {
                owner,
                window: ROOT_WINDOW,
                event_mask: 0,
            },
        );
        expect.push(id_present);

        // ── assertions ──

        for id in &expect {
            assert!(state.xid_occupied(*id), "id 0x{id:x} must be occupied");
        }
        assert_eq!(
            expect.len(),
            17,
            "one seed per namespace — update when adding namespaces"
        );
        assert!(!state.xid_occupied(base + 100), "unseeded id must be free");

        // used_xids_in returns exactly the seeded set, sorted
        let used = state.used_xids_in(base, 0x000F_FFFF);
        let mut sorted = expect.clone();
        sorted.sort_unstable();
        assert_eq!(used, sorted);
        // out-of-range base sees none of them
        assert!(state.used_xids_in(0x0020_0000, 0x000F_FFFF).is_empty());
    }
}
