//! libinput context wrapper.
//!
//! Owns an `input::Libinput` against udev seat0 with a `LibinputInterface`
//! that honours the flags libinput requests (per the libinput contract —
//! some devices are read-only, forcing O_RDWR breaks them). The context
//! exposes its fd for epoll integration and a `dispatch()` method that
//! pulls pending libinput events and translates the relevant subset to
//! [`InputEvent`].

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    hash::Hash,
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use input::{
    Device, DeviceCapability, Event, Led, Libinput, LibinputInterface,
    event::{
        EventTrait,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        pointer::{Axis, ButtonState, PointerEvent, PointerScrollEvent},
    },
};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use yserver_core::{
    core_loop::message::{
        DeviceInfo, EndpointInstanceKey, LibinputConfigSnapshot, device_node_from_sysname,
    },
    xinput::{
        InputCapabilities, InputSourceId,
        libinput_props::{DeviceConfigChange, DeviceConfigError},
    },
};

use crate::input::{event::InputEvent, libinput_config};

struct Interface;

/// Tracks the runtime source identity associated with each live backend
/// handle. Descriptive device metadata such as an evdev node is deliberately
/// not part of this mapping.
#[derive(Debug)]
struct SourceTracker<K> {
    bindings: HashMap<K, InputSourceId>,
    live_ids: HashSet<InputSourceId>,
    next_id: Option<u64>,
}

impl<K: Eq + Hash> SourceTracker<K> {
    fn new() -> Self {
        Self {
            bindings: HashMap::new(),
            live_ids: HashSet::new(),
            next_id: Some(1),
        }
    }

    fn add(&mut self, key: K) -> io::Result<InputSourceId> {
        if self.bindings.contains_key(&key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "source handle is already bound",
            ));
        }

        let next_id = self
            .next_id
            .ok_or_else(|| io::Error::other("input source identity exhausted"))?;
        let source_id = InputSourceId(next_id);
        self.next_id = next_id.checked_add(1);
        self.bindings.insert(key, source_id);
        self.live_ids.insert(source_id);
        Ok(source_id)
    }

    fn get(&self, key: &K) -> Option<InputSourceId> {
        self.bindings.get(key).copied()
    }

    fn remove(&mut self, key: &K) -> Option<InputSourceId> {
        let source_id = self.bindings.remove(key)?;
        self.live_ids.remove(&source_id);
        Some(source_id)
    }

    fn unbind_all(&mut self) {
        self.bindings.clear();
        self.live_ids.clear();
    }

    /// Bind a replacement handle to an already allocated source after a
    /// caller has independently proven a paused continuation.
    fn rebind(&mut self, key: K, source_id: InputSourceId) -> io::Result<()> {
        if self.bindings.contains_key(&key) || self.live_ids.contains(&source_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "source handle or identity is already live",
            ));
        }
        if source_id.0 == 0 || self.next_id.is_some_and(|next_id| source_id.0 >= next_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source identity was not previously allocated",
            ));
        }

        self.bindings.insert(key, source_id);
        self.live_ids.insert(source_id);
        Ok(())
    }
}

trait EndpointInstanceResolver: Send + Sync {
    fn resolve(&self, sysname: &str) -> Option<EndpointInstanceKey>;
}

struct SysfsEndpointResolver;

impl EndpointInstanceResolver for SysfsEndpointResolver {
    fn resolve(&self, sysname: &str) -> Option<EndpointInstanceKey> {
        #[cfg(target_os = "linux")]
        {
            std::fs::canonicalize(Path::new("/sys/class/input").join(sysname))
                .ok()
                .map(EndpointInstanceKey)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = sysname;
            None
        }
    }
}

trait MonotonicClock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemMonotonicClock;

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResumeWindowToken(u64);

struct ResumeWindow {
    token: ResumeWindowToken,
    deadline: Instant,
}

struct ResumeTracker {
    /// Disabled source facts kept through the bounded continuation window.
    paused: HashMap<InputSourceId, DeviceInfo>,
    active: Option<ResumeWindow>,
    next_token: Option<u64>,
    clock: Arc<dyn MonotonicClock>,
}

impl ResumeTracker {
    fn new(clock: Arc<dyn MonotonicClock>) -> Self {
        Self {
            paused: HashMap::new(),
            active: None,
            next_token: Some(1),
            clock,
        }
    }

    fn invalidate(&mut self) {
        self.active = None;
    }

    fn begin(&mut self) -> io::Result<()> {
        let next = self
            .next_token
            .ok_or_else(|| io::Error::other("resume window token exhausted"))?;
        let now = self.clock.now();
        let deadline = now.checked_add(Duration::from_millis(2500)).unwrap_or(now);
        self.next_token = next.checked_add(1);
        self.active = Some(ResumeWindow {
            token: ResumeWindowToken(next),
            deadline,
        });
        Ok(())
    }

    fn window(&self) -> Option<(ResumeWindowToken, Instant)> {
        self.active
            .as_ref()
            .map(|window| (window.token, window.deadline))
    }

    fn accepts_continuation(&self, now: Instant) -> bool {
        self.active
            .as_ref()
            .is_some_and(|window| now < window.deadline)
    }
}

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let result = OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_ACCMODE == O_RDONLY) | (flags & O_ACCMODE == O_RDWR))
            .write((flags & O_ACCMODE == O_WRONLY) | (flags & O_ACCMODE == O_RDWR))
            .open(path);
        match result {
            Ok(file) => {
                log::debug!("libinput: open_restricted ok: {}", path.display());
                Ok(file.into())
            }
            Err(err) => {
                log::warn!(
                    "libinput: open_restricted failed: {} -> {err}",
                    path.display()
                );
                Err(err.raw_os_error().unwrap_or(libc::EIO))
            }
        }
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

pub struct Context {
    libinput: Libinput,
    /// Stable identity for each live libinput device handle. Device nodes are
    /// metadata and can be reused after an attachment is removed.
    sources: SourceTracker<Device>,
    /// Latest atom-free source facts, including attachments retained while
    /// libinput is suspended. Facts are keyed only by runtime source ID.
    source_facts: HashMap<InputSourceId, DeviceInfo>,
    resume_tracker: ResumeTracker,
    endpoint_resolver: Arc<dyn EndpointInstanceResolver>,
    /// Live configurable pointer handles keyed by libinput handle identity.
    /// The device-node string is retained only for the legacy config-write
    /// API until the inventory migration replaces that selector.
    ///
    /// `input::Device` is refcounted at the C level (`libinput_device_ref`)
    /// and the Rust wrapper exposes that via `Clone` — stashing the handle
    /// here keeps the device alive even after libinput's own iterator
    /// drops its borrow, and the entry's eventual `remove(...)` drops
    /// the last ref.
    pointer_devices: HashMap<Device, (String, Device)>,
    /// Live handles for keyboard-capability devices, keyed by libinput
    /// handle identity. Consumed by
    /// [`Context::update_leds`] — the XKB lock state (Caps/Num/Scroll)
    /// lives in the server core, so the server must push LED changes
    /// down to the hardware via `libinput_device_led_update`; nothing
    /// else will (this is the KMS server, there is no other driver).
    keyboard_devices: HashMap<Device, Device>,
    /// Last LED mask applied — re-applied to keyboards that appear
    /// later (hotplug, VT-switch re-acquire re-adds devices with their
    /// LEDs reset).
    last_leds: Led,
    /// Handles of currently-open **keyboard- or pointer-capable**
    /// devices. The startup guard requires this to be non-empty: a session
    /// whose only opened device is non-usable (e.g. a lone HID "System
    /// Control" collection that opened while the real keyboard/mouse were
    /// seat-denied) is dead on arrival and can't even be zapped. Add/remove
    /// tracked so the count stays accurate across hotplug.
    usable_input_devices: HashSet<Device>,
}

/// Newtype wrapper around `Context` that implements `Send`.
/// SAFETY: The libinput thread is the sole owner. We need `Send` only
/// because the context crosses the spawn boundary into that thread.
pub struct SendContext(Context);
unsafe impl Send for SendContext {}

impl SendContext {
    pub fn new() -> io::Result<Self> {
        Context::new().map(Self)
    }

    pub fn fd(&self) -> RawFd {
        self.0.fd()
    }

    pub fn dispatch(&mut self) -> io::Result<Vec<InputEvent>> {
        self.0.dispatch()
    }

    /// Number of open keyboard/pointer-capable devices (startup guard).
    pub fn usable_input_device_count(&self) -> usize {
        self.0.usable_input_device_count()
    }

    pub fn update_leds(&mut self, leds: Led) {
        self.0.update_leds(leds);
    }

    pub fn suspend(&mut self) -> Vec<InputEvent> {
        self.0.suspend()
    }

    pub fn resume(&mut self) -> io::Result<Vec<InputEvent>> {
        self.0.resume()
    }

    pub(crate) fn resume_window(&self) -> Option<(ResumeWindowToken, Instant)> {
        self.0.resume_window()
    }

    pub(crate) fn finish_resume_window(
        &mut self,
        token: ResumeWindowToken,
        now: Instant,
    ) -> Vec<InputEvent> {
        self.0.finish_resume_window(token, now)
    }

    /// Route a `xinput set-prop` write to the wrapped libinput context.
    /// The input thread owns the live device map, so client device-config
    /// writes (forwarded over `InputThreadControl`) land via this forward.
    ///
    /// # Errors
    ///
    /// Propagates libinput's [`DeviceConfigError`] (Unsupported / Invalid)
    /// from the inner [`Context::apply_device_config`].
    pub fn apply_device_config(
        &mut self,
        source: InputSourceId,
        change: DeviceConfigChange,
    ) -> Result<(), DeviceConfigError> {
        self.0.apply_device_config(source, change)
    }

    /// Whether a submitted command may wait for this source's proven
    /// continuation during VT recovery.
    pub(crate) fn can_wait_for_device_config(&self, source: InputSourceId) -> bool {
        self.0.can_wait_for_device_config(source)
    }
}

impl AsFd for SendContext {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Context {
    pub fn new() -> io::Result<Self> {
        Self::new_with_runtime(
            Arc::new(SysfsEndpointResolver),
            Arc::new(SystemMonotonicClock),
        )
    }

    fn new_with_runtime(
        endpoint_resolver: Arc<dyn EndpointInstanceResolver>,
        clock: Arc<dyn MonotonicClock>,
    ) -> io::Result<Self> {
        // Access check (always-Direct: no libseat to grant device access).
        // If input nodes exist but any is permission-denied, yserver lacks
        // input access — the real keyboard/mouse won't open even if an odd
        // node does. Fail here so startup refuses (routed via the no-input
        // abort) instead of coming up with a dead, un-zappable session.
        let (present, permission_denied) = probe_input_devnodes();
        if present > 0 && permission_denied > 0 {
            return Err(io::Error::other(format!(
                "cannot open input devices: {permission_denied} of {present} \
                 /dev/input/event* nodes are permission-denied.\n\
                 yserver has no seat/libseat; it needs direct access to input \
                 devices — add the user to the 'input' group (or grant the \
                 seat ACL) and run from the console, not over SSH."
            )));
        }
        let mut libinput = Libinput::new_with_udev(Interface);
        libinput.udev_assign_seat("seat0").map_err(|()| {
            io::Error::other(
                "libinput: udev_assign_seat(\"seat0\") failed — is udev running and the \
                 seat reachable from this process?",
            )
        })?;
        Ok(Self {
            libinput,
            sources: SourceTracker::new(),
            source_facts: HashMap::new(),
            resume_tracker: ResumeTracker::new(clock),
            endpoint_resolver,
            pointer_devices: HashMap::new(),
            keyboard_devices: HashMap::new(),
            last_leds: Led::empty(),
            usable_input_devices: HashSet::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.libinput.as_raw_fd()
    }

    /// Count of currently-open **keyboard- or pointer-capable** devices.
    /// The startup guard requires this to be ≥1 — a session with input
    /// devices that are none of keyboard/pointer (e.g. only a HID "System
    /// Control" node) is unusable. See [`usable_input_devices`](Self).
    pub fn usable_input_device_count(&self) -> usize {
        self.usable_input_devices.len()
    }

    pub fn dispatch(&mut self) -> io::Result<Vec<InputEvent>> {
        self.libinput.dispatch()?;
        let mut out = Vec::new();
        for event in &mut self.libinput {
            // Log device add/remove unconditionally so we can tell from
            // the server log whether libinput is seeing input hardware.
            // No devices ever logged → seat permission / udev issue.
            match &event {
                Event::Device(input::event::DeviceEvent::Added(d)) => {
                    let mut dev = d.device();
                    let name = dev.name().into_owned();
                    let tap_finger_count = dev.config_tap_finger_count();
                    let is_tp = is_touchpad(tap_finger_count);
                    let sysname = dev.sysname().to_owned();
                    let resume_key = self.endpoint_resolver.resolve(&sysname);
                    if resume_key.is_none()
                        && self
                            .resume_tracker
                            .accepts_continuation(self.resume_tracker.clock.now())
                    {
                        log::warn!(
                            "libinput: cannot prove endpoint instance for {sysname:?} during VT recovery; adding a fresh source"
                        );
                    }
                    let continuation = resume_key.as_ref().and_then(|key| {
                        if !self
                            .resume_tracker
                            .accepts_continuation(self.resume_tracker.clock.now())
                        {
                            return None;
                        }
                        self.resume_tracker
                            .paused
                            .iter()
                            .find(|(_, info)| info.resume_key.as_ref() == Some(key))
                            .map(|(source_id, _)| *source_id)
                    });
                    let mut continued_source_id = None;
                    let source_id = if let Some(source_id) = continuation {
                        match self.sources.rebind(dev.clone(), source_id) {
                            Ok(()) => {
                                self.resume_tracker.paused.remove(&source_id);
                                continued_source_id = Some(source_id);
                                source_id
                            }
                            Err(err) => {
                                log::warn!(
                                    "libinput: failed to rebind endpoint {sysname:?} to source {}: {err}; adding a fresh source",
                                    source_id.0
                                );
                                self.sources.add(dev.clone())?
                            }
                        }
                    } else {
                        self.sources.add(dev.clone())?
                    };
                    let saved_config = continued_source_id
                        .and_then(|source_id| self.source_facts.get(&source_id))
                        .map(|info| info.config);
                    if is_tp {
                        configure_touchpad(&mut dev, &name);
                    }
                    if let Some(config) = saved_config {
                        restore_config_snapshot(&mut dev, &name, config);
                    }
                    if is_tp {
                        log::info!(
                            "libinput: device added: {name:?} (touchpad — tap-to-click + \
                             disable-while-typing enabled)"
                        );
                    } else {
                        log::info!("libinput: device added: {name:?}");
                    }
                    // Prefer the real udev devnode; fall back to the
                    // derived path (libinput sysname == `eventN`, so
                    // the node is always `/dev/input/eventN`).
                    let device_node = {
                        // SAFETY: libinput holds the udev device alive
                        // for the duration of this event; we only read
                        // the devnode string and drop the handle.
                        let node = unsafe { dev.udev_device() }
                            .and_then(|ud| ud.devnode().map(|p| p.to_string_lossy().into_owned()));
                        node.unwrap_or_else(|| device_node_from_sysname(&sysname))
                    };
                    // T4: gather the live config snapshot for any pointer
                    // device (touchpad OR mouse) so the XI2 property registry
                    // exposes which libinput knobs are available / current /
                    // default on it. A mouse still has accel / left-handed /
                    // natural-scroll / send-events knobs — the KDE Mouse KCM
                    // reads `libinput Accel Speed` and SIGSEGVs if the atom is
                    // absent. Non-pointer devices (keyboards) keep the
                    // all-`false` default snapshot.
                    let is_pointer = dev.has_capability(DeviceCapability::Pointer);
                    let is_keyboard = dev.has_capability(DeviceCapability::Keyboard);
                    let is_touch = dev.has_capability(DeviceCapability::Touch);
                    let config = if is_tp || is_pointer {
                        libinput_config::gather(&dev)
                    } else {
                        LibinputConfigSnapshot::default()
                    };
                    // Retain every pointer-capable handle so its full
                    // recognized snapshot can be refreshed after successful
                    // config writes and restored after a proven continuation.
                    // Device identity, not the event-node string, joins it to
                    // the source fact.
                    if is_pointer || is_tp {
                        self.pointer_devices
                            .insert(dev.clone(), (device_node.clone(), dev.clone()));
                    }
                    // Keyboard-capability devices are stashed for LED
                    // writes (update_leds). Re-apply the current lock-
                    // LED mask to a newly-appearing keyboard: hotplug
                    // and VT-switch re-acquire re-add devices with
                    // their LEDs reset, but the X-side lock state
                    // persists.
                    if is_keyboard {
                        // Force the device to the current lock state
                        // unconditionally — including all-off — so a
                        // keyboard that appears with a stale firmware
                        // LED (e.g. a BIOS-lit NumLock) is corrected to
                        // match the server, not just keyboards added
                        // while a lock happens to be active.
                        dev.led_update(self.last_leds);
                        self.keyboard_devices.insert(dev.clone(), dev.clone());
                    }
                    // Track keyboard/pointer-capable devices for the startup
                    // usable-input guard (`usable_input_device_count`). A lone
                    // non-usable device (e.g. a HID "System Control" collection
                    // that opens while the real keyboard/mouse are seat-denied)
                    // must NOT count as usable input.
                    if is_keyboard || is_pointer {
                        self.usable_input_devices.insert(dev.clone());
                    }
                    let info = DeviceInfo {
                        source_id,
                        enabled: true,
                        resume_key,
                        capabilities: InputCapabilities {
                            keyboard: is_keyboard,
                            pointer: is_pointer,
                            touch: is_touch,
                        },
                        name,
                        device_node,
                        sysname,
                        vendor_id: dev.id_vendor(),
                        product_id: dev.id_product(),
                        is_touchpad: is_tp,
                        config,
                    };
                    self.source_facts.insert(source_id, info.clone());
                    if continued_source_id == Some(source_id) {
                        out.push(InputEvent::DeviceResumed(info));
                    } else {
                        out.push(InputEvent::DeviceAdded(info));
                    }
                }
                Event::Device(input::event::DeviceEvent::Removed(d)) => {
                    let dev = d.device();
                    let name = dev.name();
                    log::info!("libinput: device removed: {name:?}");
                    let Some(source_id) = self.sources.remove(&dev) else {
                        log::debug!(
                            "libinput: ignoring removal for unknown device handle {name:?}"
                        );
                        continue;
                    };
                    // Drop any configuration/LED handles for this exact
                    // attachment (libinput unrefs them via Drop).
                    self.pointer_devices.remove(&dev);
                    self.keyboard_devices.remove(&dev);
                    self.usable_input_devices.remove(&dev);
                    self.source_facts.remove(&source_id);
                    self.resume_tracker.paused.remove(&source_id);
                    out.push(InputEvent::DeviceRemoved { source_id });
                }
                _ => {}
            }
            let event_device = event.device();
            if let Some(source_id) = self.sources.get(&event_device)
                && let Some(translated) = translate(&event, source_id)
            {
                out.push(translated);
            }
        }
        Ok(out)
    }

    /// Push the X-side lock-LED state (Caps/Num/Scroll) to every keyboard
    /// device. Called from the input thread after a [`crate::input::LedRelay`]
    /// wakeup. Also remembered for keyboards that appear later (see the
    /// DeviceAdded arm).
    pub fn update_leds(&mut self, leds: Led) {
        self.last_leds = leds;
        for dev in self.keyboard_devices.values_mut() {
            dev.led_update(leds);
        }
    }

    /// Route a decoded `xinput set-prop` / KCM `XIChangeProperty` write
    /// through to the live libinput device for its `InputSourceId`. The XI
    /// pointer facet identifies that source; configuration stays per physical
    /// source so a write to a mouse cannot touch a touchpad.
    ///
    /// Each physical pointer has its own registry-assigned XI facet and
    /// configuration. The reserved XTEST pointer (id 4) has no libinput
    /// source ID, so it cannot name a physical configuration target.
    ///
    /// Returns [`DeviceConfigError::SourceGone`] when the source ID no longer
    /// has a live pointer handle. The input thread may retain a request across
    /// a confirmed resume; a removed source completes with `SourceGone`.
    ///
    /// Errors map libinput's [`input::DeviceConfigError`] onto the
    /// X-layer's [`DeviceConfigError`]: `Unsupported` → BadMatch,
    /// `Invalid` → BadValue (the mapping is performed by the
    /// `dispatch_change_property` helper that calls us).
    ///
    /// # Errors
    ///
    /// Returns [`DeviceConfigError::Unsupported`] when libinput
    /// reports the setting isn't available on this device, or
    /// [`DeviceConfigError::Invalid`] when the value is out of range.
    /// Returns [`DeviceConfigError::SourceGone`] when `source` has no live
    /// pointer handle.
    pub fn apply_device_config(
        &mut self,
        source: InputSourceId,
        change: DeviceConfigChange,
    ) -> Result<(), DeviceConfigError> {
        for (handle, (_, dev)) in &mut self.pointer_devices {
            if self.sources.get(handle) == Some(source) {
                libinput_config::apply(dev, change)?;
                if let Some(info) = self.source_facts.get_mut(&source) {
                    info.config = libinput_config::gather(dev);
                }
                return Ok(());
            }
        }
        Err(DeviceConfigError::SourceGone)
    }

    fn can_wait_for_device_config(&self, source: InputSourceId) -> bool {
        self.resume_tracker
            .paused
            .get(&source)
            .is_some_and(|info| info.resume_key.is_some())
    }
}

impl AsFd for Context {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.libinput.as_fd()
    }
}

/// A libinput device is a touchpad iff it reports a tap finger count.
/// libinput/wlroots classify touchpads this way: pointers that are not
/// touchpads (mice, trackpoints) report a finger count of 0, while
/// clickpads/touchpads report >= 1. We use this to decide whether to
/// apply touchpad-friendly defaults at device-add time.
fn is_touchpad(tap_finger_count: u32) -> bool {
    tap_finger_count > 0
}

/// Apply touchpad-friendly defaults at device-add so the laptop is
/// usable without a settings daemon. libinput defaults tapping OFF, so
/// without this "tap to click" does nothing on a fresh yserver session
/// (the reported yoga symptom). We also enable disable-while-typing to
/// suppress accidental cursor jumps while typing. Scroll direction is
/// left at the libinput default to avoid surprising the user by
/// reversing it. Errors are logged, not fatal — a touchpad that rejects
/// a config still works, just without that nicety.
fn configure_touchpad(dev: &mut Device, name: &str) {
    if let Err(e) = dev.config_tap_set_enabled(true) {
        log::warn!("libinput: enable tap-to-click on {name:?} failed: {e:?}");
    }
    if let Err(e) = dev.config_dwt_set_enabled(true) {
        // Many touchpads don't support DWT; that's expected, so debug.
        log::debug!("libinput: disable-while-typing on {name:?} unavailable: {e:?}");
    }
}

/// Reapply the confirmed pre-suspend libinput values after ordinary
/// touchpad setup has run. The returned add/resume snapshot is gathered
/// afterward, so failed setters are represented by the actual device state.
fn restore_config_snapshot(dev: &mut Device, name: &str, config: LibinputConfigSnapshot) {
    use DeviceConfigChange as C;

    let mut changes = Vec::new();
    changes.extend([
        config.tap.available.then_some(C::Tap(config.tap.current)),
        config
            .tap_drag
            .available
            .then_some(C::TapDrag(config.tap_drag.current)),
        config
            .tap_drag_lock
            .available
            .then_some(C::TapDragLock(config.tap_drag_lock.current)),
        config
            .natural_scroll
            .available
            .then_some(C::NaturalScroll(config.natural_scroll.current)),
        config.dwt.available.then_some(C::Dwt(config.dwt.current)),
        config
            .left_handed
            .available
            .then_some(C::LeftHanded(config.left_handed.current)),
        config
            .middle_emulation
            .available
            .then_some(C::MiddleEmulation(config.middle_emulation.current)),
        config
            .scroll_button_lock
            .available
            .then_some(C::ScrollButtonLock(config.scroll_button_lock.current)),
        config
            .accel
            .available
            .then_some(C::AccelSpeed(config.accel.current)),
        config
            .scroll_button
            .available
            .then_some(C::ScrollButton(config.scroll_button.current)),
        (config.scroll_method.available_mask != 0)
            .then_some(C::ScrollMethod(config.scroll_method.current)),
        config
            .click_method
            .available
            .then_some(C::ClickMethod(config.click_method.current)),
        config
            .accel_profile
            .available
            .then_some(C::AccelProfile(config.accel_profile.current)),
        (config.send_events.available_mask != 0)
            .then_some(C::SendEvents(config.send_events.current_mask)),
    ]);
    if config.tap_button_map.available
        && let Some(index) = config.tap_button_map.current
    {
        changes.push(Some(C::TapButtonMap(index)));
    }

    for change in changes.into_iter().flatten() {
        if let Err(err) = libinput_config::apply(dev, change) {
            log::warn!(
                "libinput: restoring saved config for {name:?} rejected ({change:?}): {err:?}"
            );
        }
    }
}

impl Context {
    /// Suspend libinput: closes all open input device fds. The context remains
    /// valid and can be resumed with [`Context::resume`]. Device facts remain
    /// keyed by source while their live libinput bindings are retired.
    pub fn suspend(&mut self) -> Vec<InputEvent> {
        self.resume_tracker.invalidate();
        let mut active: Vec<(Device, InputSourceId)> = self
            .sources
            .bindings
            .iter()
            .map(|(device, source_id)| (device.clone(), *source_id))
            .collect();
        active.sort_unstable_by_key(|(_, source_id)| source_id.0);
        let mut out = Vec::with_capacity(active.len());
        for (device, source_id) in active {
            let Some(mut info) = self.source_facts.get(&source_id).cloned() else {
                continue;
            };
            if let Some((_, pointer_device)) = self.pointer_devices.get(&device) {
                info.config = libinput_config::gather(pointer_device);
            }
            info.enabled = false;
            self.source_facts.insert(source_id, info.clone());
            self.resume_tracker.paused.insert(source_id, info.clone());
            out.push(InputEvent::DeviceSuspended { source_id });
        }

        // Clear only live handle bindings. The source allocator and paused
        // facts are process-lifetime and remain available for continuation.
        self.sources.unbind_all();
        self.pointer_devices.clear();
        self.keyboard_devices.clear();
        self.usable_input_devices.clear();
        self.libinput.suspend();
        out
    }

    /// Resume a suspended libinput context. Re-enables device monitoring and
    /// re-opens devices via `open_restricted`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `libinput_resume` returns -1.
    pub fn resume(&mut self) -> io::Result<Vec<InputEvent>> {
        if self.resume_tracker.next_token.is_none() {
            return Err(io::Error::other("resume window token exhausted"));
        }
        self.libinput
            .resume()
            .map_err(|()| io::Error::other("libinput resume failed"))?;
        self.resume_tracker.begin()?;

        let source_facts_before = self.source_facts.clone();
        let paused_before = self.resume_tracker.paused.clone();
        let mut out = match self.dispatch() {
            Ok(events) => events,
            Err(err) => {
                self.resume_tracker.invalidate();
                self.resume_tracker.paused = paused_before;
                self.source_facts = source_facts_before;
                // Keep any newly allocated IDs burned, but retire every
                // partially rebound handle from this failed enumeration.
                self.sources.unbind_all();
                self.pointer_devices.clear();
                self.keyboard_devices.clear();
                self.usable_input_devices.clear();
                self.libinput.suspend();
                return Err(err);
            }
        };

        // A source without a captured sysfs instance cannot be proven to be
        // the same endpoint. Retire it as a removal; a newly enumerated
        // handle receives a fresh source ID in the dispatch batch above.
        let unprovable: Vec<InputSourceId> = self
            .resume_tracker
            .paused
            .iter()
            .filter_map(|(source_id, info)| info.resume_key.is_none().then_some(*source_id))
            .collect();
        let mut removals = Vec::with_capacity(unprovable.len());
        for source_id in unprovable {
            if let Some(info) = self.resume_tracker.paused.remove(&source_id) {
                log::warn!(
                    "libinput: cannot prove VT continuation for source {} node={}; removing old source",
                    source_id.0,
                    info.device_node
                );
                self.source_facts.remove(&source_id);
                removals.push(InputEvent::DeviceRemoved { source_id });
            }
        }
        removals.append(&mut out);
        Ok(removals)
    }

    pub(crate) fn resume_window(&self) -> Option<(ResumeWindowToken, Instant)> {
        self.resume_tracker.window()
    }

    pub(crate) fn finish_resume_window(
        &mut self,
        token: ResumeWindowToken,
        now: Instant,
    ) -> Vec<InputEvent> {
        let Some(window) = self.resume_tracker.active.as_ref() else {
            return Vec::new();
        };
        if window.token != token || now < window.deadline {
            return Vec::new();
        }
        self.resume_tracker.active = None;
        let mut sources: Vec<InputSourceId> = self.resume_tracker.paused.keys().copied().collect();
        sources.sort_unstable_by_key(|source_id| source_id.0);
        sources
            .into_iter()
            .filter_map(|source_id| {
                self.resume_tracker.paused.remove(&source_id)?;
                self.source_facts.remove(&source_id);
                Some(InputEvent::DeviceRemoved { source_id })
            })
            .collect()
    }
}

/// Best-effort `/dev/input/` enumeration logged at startup. Lets us
/// tell from the log whether the input nodes exist and whether our
/// process can stat / open them. udev rules from logind grant ACL on
/// `event*` to the active session; if we see `open: ok` here but
/// libinput's `open_restricted` fails, the seat is the wrong one.
/// Probe every `/dev/input/event*` node with an `O_RDONLY` open, logging
/// each result, and return `(present, permission_denied)`.
///
/// This is the access check that matters now that yserver is always Direct
/// (no libseat): a session with input access (in the `input` group / holding
/// the seat's ACL) can open every input node. Any `EACCES`/`EPERM` here means
/// yserver does NOT have input access — the real keyboard/mouse won't work even
/// if some odd node (e.g. a HID "System Control" collection, which libinput
/// still reports as keyboard-capable) happens to open.
fn probe_input_devnodes() -> (usize, usize) {
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(err) => {
            log::warn!("/dev/input: read_dir failed: {err}");
            return (0, 0);
        }
    };
    let mut nodes: Vec<_> = dir.flatten().collect();
    nodes.sort_by_key(std::fs::DirEntry::file_name);
    let mut present = 0usize;
    let mut permission_denied = 0usize;
    for entry in nodes {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("event") {
            continue;
        }
        present += 1;
        let path = entry.path();
        match OpenOptions::new().read(true).open(&path) {
            Ok(_f) => log::debug!("/dev/input/{name_str}: open(O_RDONLY) ok"),
            Err(err) => {
                if matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM)) {
                    permission_denied += 1;
                }
                log::warn!("/dev/input/{name_str}: open(O_RDONLY) failed: {err}");
            }
        }
    }
    (present, permission_denied)
}

/// Finger/continuous scroll → `PointerScroll` v120 quantization.
/// Both event types expose only `scroll_value` (in cursor-pixel-
/// equivalent units, no v120 quantization). Convert at ~15 px per
/// logical wheel click (xwayland/Sway convention) → factor 8.
///
/// `has_axis(axis)` MUST be checked first: libinput emits a
/// `client bug: value requested for unset axis` error if
/// `scroll_value` is called for an axis the event doesn't carry.
fn finger_or_continuous_to_event<E>(ev: &E, source_id: InputSourceId) -> Option<InputEvent>
where
    E: PointerScrollEvent,
{
    const PX_TO_V120: f64 = 8.0;
    let dx_v120 = if ev.has_axis(Axis::Horizontal) {
        (ev.scroll_value(Axis::Horizontal) * PX_TO_V120) as i32
    } else {
        0
    };
    let dy_v120 = if ev.has_axis(Axis::Vertical) {
        (ev.scroll_value(Axis::Vertical) * PX_TO_V120) as i32
    } else {
        0
    };
    if dx_v120 == 0 && dy_v120 == 0 {
        return None;
    }
    Some(InputEvent::PointerScroll {
        source_id,
        dx_v120,
        dy_v120,
    })
}

fn translate(event: &Event, source_id: InputSourceId) -> Option<InputEvent> {
    match event {
        Event::Keyboard(KeyboardEvent::Key(key)) => {
            let keycode = key.key();
            Some(match key.key_state() {
                KeyState::Pressed => InputEvent::KeyPress { source_id, keycode },
                KeyState::Released => InputEvent::KeyRelease { source_id, keycode },
            })
        }
        Event::Pointer(PointerEvent::Motion(motion)) => Some(InputEvent::PointerMotion {
            source_id,
            dx: motion.dx(),
            dy: motion.dy(),
        }),
        Event::Pointer(PointerEvent::MotionAbsolute(motion)) => {
            // libinput's `absolute_x/y_transformed(W)` maps the device's full
            // axis range to `0..W`.  Pass a large W and divide to recover a
            // normalised 0..1 coordinate; the backend scales to scanout size.
            const SCALE: u32 = 1_000_000;
            Some(InputEvent::PointerMotionAbsolute {
                source_id,
                x_norm: motion.absolute_x_transformed(SCALE) / SCALE as f64,
                y_norm: motion.absolute_y_transformed(SCALE) / SCALE as f64,
            })
        }
        Event::Pointer(PointerEvent::Button(btn)) => Some(InputEvent::Button {
            source_id,
            code: btn.button(),
            pressed: btn.button_state() == ButtonState::Pressed,
        }),
        Event::Pointer(PointerEvent::ScrollWheel(ev)) => {
            // Wheel events come pre-quantized in v120 (120 = one click).
            // has_axis(axis) MUST be checked first: libinput emits a
            // `client bug: value requested for unset axis` error if
            // scroll_value_v120 is called for an axis the event doesn't
            // carry. A pure vertical wheel event has Horizontal unset.
            let dx_v120 = if ev.has_axis(Axis::Horizontal) {
                ev.scroll_value_v120(Axis::Horizontal) as i32
            } else {
                0
            };
            let dy_v120 = if ev.has_axis(Axis::Vertical) {
                ev.scroll_value_v120(Axis::Vertical) as i32
            } else {
                0
            };
            if dx_v120 == 0 && dy_v120 == 0 {
                return None;
            }
            Some(InputEvent::PointerScroll {
                source_id,
                dx_v120,
                dy_v120,
            })
        }
        // ScrollFinger: a zero-delta event is libinput's fingers-lifted stop
        // (`finger_or_continuous_to_event` returns None only for all-zero
        // deltas), which we surface as PointerScrollStop. ScrollContinuous has
        // no finger-lift, so its zero deltas stay dropped.
        Event::Pointer(PointerEvent::ScrollFinger(ev)) => Some(
            finger_or_continuous_to_event(ev, source_id)
                .unwrap_or(InputEvent::PointerScrollStop { source_id }),
        ),
        Event::Pointer(PointerEvent::ScrollContinuous(ev)) => {
            finger_or_continuous_to_event(ev, source_id)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{MonotonicClock, ResumeTracker, is_touchpad};
    use std::{
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    /// Touchpad classification keys off libinput's tap finger count:
    /// mice / trackpoints / keyboards report 0; clickpads/touchpads
    /// report >= 1. (The config application itself is libinput FFI,
    /// verified on hardware — only the decision is unit-testable.)
    #[test]
    fn touchpad_classified_by_tap_finger_count() {
        assert!(!is_touchpad(0), "0 fingers = not a touchpad");
        assert!(is_touchpad(1), "1 finger = touchpad");
        assert!(is_touchpad(3), "3 fingers = touchpad");
    }

    struct TestClock(Mutex<Instant>);

    impl MonotonicClock for TestClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn xi_dynamic_reset_keeps_active_vt_recovery_window() {
        use yserver_core::{
            core_loop::{
                DeviceInfo, EndpointInstanceKey, HostInputEvent, Message, ResetPolicy, channel,
                message::LibinputConfigSnapshot, run_core,
            },
            xinput::{InputCapabilities, InputSourceId},
        };

        let now = Instant::now();
        let clock = Arc::new(TestClock(Mutex::new(now)));
        let mut recovery = ResumeTracker::new(clock);
        let source_id = InputSourceId(91);
        let paused_info = DeviceInfo {
            source_id,
            enabled: false,
            resume_key: Some(EndpointInstanceKey("input-91".into())),
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: true,
                touch: false,
            },
            name: "Paused mixed source".into(),
            device_node: "/dev/input/event91".into(),
            sysname: "event91".into(),
            vendor_id: 1,
            product_id: 91,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        };
        recovery.paused.insert(source_id, paused_info.clone());
        recovery
            .begin()
            .expect("start the active VT recovery window");
        let active_window = recovery.window().expect("active resume deadline");

        let (poll, sender, receiver) = channel().expect("core channel");
        let generations = receiver.generation_counter();
        let before = generations.current();
        let input_sender = sender.clone_handle();
        let driver = thread::spawn(move || {
            input_sender
                .send(Message::HostInput(HostInputEvent::DeviceAdded(paused_info)))
                .expect("publish suspended source to the process-lifetime inventory");
            input_sender
                .send(Message::ResetRequested)
                .expect("request reset while VT recovery is active");
            let deadline = Instant::now() + Duration::from_secs(5);
            while generations.current() == before && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            assert_ne!(
                generations.current(),
                before,
                "runner crossed the reset boundary"
            );
            input_sender
                .send(Message::Shutdown)
                .expect("stop the runner after reset");
        });

        let mut state = yserver_core::server::ServerState::new();
        let mut backend = crate::kms::render::KmsBackend::for_tests();
        backend.platform.devices.clear();
        run_core(
            poll,
            receiver,
            sender,
            &mut state,
            &mut backend,
            Vec::new(),
            &yserver_core::core_loop::poll_tokens::ClientIdAllocator::new(),
            yserver_core::core_loop::auth::AuthState::new(None),
            ResetPolicy::Reset,
            None,
        )
        .expect("runner reset succeeds during the active recovery window");
        driver.join().expect("reset driver thread");

        assert_eq!(recovery.window(), Some(active_window));
        assert_eq!(
            recovery.paused.get(&source_id).map(|info| info.enabled),
            Some(false)
        );
        assert!(!state.xi_devices.source(source_id).unwrap().enabled);
        assert!(
            state
                .xi_devices
                .facet(source_id, yserver_core::xinput::XiFacetKind::Keyboard)
                .is_some()
        );
        assert!(
            state
                .xi_devices
                .facet(source_id, yserver_core::xinput::XiFacetKind::PointerTouch)
                .is_some()
        );
    }
}
