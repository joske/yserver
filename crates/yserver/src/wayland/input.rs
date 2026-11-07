//! The compositor's seat, into X: keys, the pointer, the wheel, the
//! keyboard focus, the cursor and a game's pointer lock.
//!
//! The compositor tells a client about its own windows only: which one the
//! pointer is on and where in it, which one has the keyboard. The core's
//! rootless seat turns that into what an X server that owns the screen would
//! have seen (`yserver_core::rootless`, spec §4.4):
//!
//! * **The pointer** is a position on a top-level's native window, which is
//!   its whole backing at buffer scale 1, so surface-local logical pixels
//!   are backing pixels: `RootlessInput::PointerEnter`, `PointerMotionLocal`
//!   and `PointerLeave`. **Buttons** pass through as evdev codes. **The
//!   wheel** becomes X's buttons 4 to 7, one press and release a click: the
//!   compositor's clicks where it counts them, else ten units of its scroll
//!   a click, which is what a wheel click is in `wl_pointer.axis`.
//! * **Keys** are evdev codes plus 8, cooked by the server's own XKB state,
//!   which took the compositor's layout at start. The X server repeats a
//!   held key itself.
//! * **Focus.** The keyboard entering a window is `HostRequest::Activated`,
//!   leaving it `Deactivated`.
//! * **The cursor** is the X cursor in effect, given with
//!   `wl_pointer.set_cursor` whenever it changes, premultiplied.
//! * **The devices.** The seat is a slave pointer and a slave keyboard of
//!   the core pair, as Xwayland's `xwayland-pointer` and
//!   `xwayland-keyboard` are, and its input comes from them.
//! * **The input's time** is one rule for keys, buttons, the wheel,
//!   motion and crossings: the server's clock as the host hands the event
//!   to X, in the order the compositor sent them, as Xwayland stamps its
//!   seat's input. The time a device stamped a relative motion with is
//!   not used (it is logged at trace level, as the input's age).
//! * **A game's pointer lock** (`RootlessEvent::PointerLock`, the core's
//!   decision): the compositor's `zwp_locked_pointer_v1` on the game's
//!   window, asked for as `persistent`, as Xwayland asks; while it is in
//!   force each relative motion is `RootlessInput::PointerRelative`, from
//!   the `zwp_relative_pointer_v1` the host asks for on the seat's pointer.

use std::collections::{BTreeSet, HashMap};

use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{
        zwp_locked_pointer_v1::ZwpLockedPointerV1,
        zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::ZwpRelativePointerV1,
    },
};
use yserver_core::{
    backend::ActiveCursorImage,
    core_loop::{
        DeviceInfo, HostInputEvent, InputOrigin,
        message::{
            LibinputConfigSnapshot, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_RIGHT,
            SYNTH_SCROLL_UP,
        },
    },
    host_x11::HostKeyEvent,
    rootless::{HostRequest, RootlessInput, ToplevelId},
    xinput::{InputCapabilities, InputSourceId},
};

use super::client::{Client, Event, KeyboardEvent, PointerEvent, SurfaceId};
use crate::rootless::{display::HostWork, present_clock::monotonic_us};

/// The compositor's seat as X sees it: a slave pointer and a slave
/// keyboard of the core pair, announced before any input
/// ([`announcements`]). Their input carries their own ids as its source, as
/// any slave's does: no X server sends an event sourced from a master, and
/// a client that opens an event's source (Wine opens a raw button's with
/// XOpenDevice) is refused a master.
///
/// The ids are the top of the source space, clear of libinput's, which
/// count up from 1.
const POINTER: InputSourceId = InputSourceId(u64::MAX - 1);
const KEYBOARD: InputSourceId = InputSourceId(u64::MAX);

/// The origin of the compositor's pointer input.
const ORIGIN: InputOrigin = InputOrigin::Physical(POINTER);

/// The origin of the compositor's keys.
const KEY_ORIGIN: InputOrigin = InputOrigin::Physical(KEYBOARD);

/// The seat's two devices, for the core before any input: XI slaves of the
/// core pointer and keyboard, as the window-system shells' are
/// (`surface/input/devices.rs`).
pub(super) fn announcements() -> [HostInputEvent; 2] {
    let device = |source_id, name: &str, keyboard| {
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard,
                pointer: !keyboard,
                touch: false,
            },
            name: name.to_owned(),
            device_node: String::new(),
            sysname: String::new(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        })
    };
    [
        device(POINTER, "Wayland pointer", false),
        device(KEYBOARD, "Wayland keyboard", true),
    ]
}

/// How much `wl_pointer.axis` scroll is one wheel click, where the
/// compositor does not count clicks itself.
const SCROLL_PER_CLICK: f64 = 10.0;

/// A cursor as given: its X cursor, its version and its hotspot.
type CursorKey = (u32, u32, (u16, u16));

/// A `zwp_locked_pointer_v1` the host asked for.
#[derive(Debug)]
struct Lock {
    object: ZwpLockedPointerV1,
    id: ToplevelId,
    /// The compositor said `locked` and not `unlocked` since.
    active: bool,
}

/// The seat's state between dispatches.
#[derive(Debug, Default)]
pub(super) struct Seat {
    /// What the compositor said since the last [`Seat::deliver`].
    queue: Vec<Event>,
    /// The compositor's window the pointer is on.
    pointer: Option<SurfaceId>,
    /// Where on it, in its surface's pixels.
    at: (f64, f64),
    /// Evdev buttons the server has down.
    buttons: BTreeSet<u32>,
    /// X keycodes the server has down.
    keys: BTreeSet<u8>,
    /// Scroll that is not yet a whole click: down, right.
    scroll: (f64, f64),
    /// The cursor last given: its version and hotspot, or none; `None`
    /// before the first.
    cursor: Option<Option<CursorKey>>,
    relative_manager: Option<ZwpRelativePointerManagerV1>,
    /// The `zwp_relative_pointer_v1` on the seat's pointer.
    relative: Option<ZwpRelativePointerV1>,
    /// The relative motion since a pointer motion was last handed to X: the
    /// latest one's time in microseconds of `CLOCK_MONOTONIC`.
    moved: Option<u64>,
    /// The compositor's `zwp_pointer_constraints_v1`, where it serves one.
    constraints: Option<ZwpPointerConstraintsV1>,
    lock: Option<Lock>,
    /// The window the compositor's keyboard is on.
    keyboard_on: Option<ToplevelId>,
}

impl Seat {
    /// Bind the compositor's relative pointer manager and pointer
    /// constraints, where it has them.
    pub(super) fn bind(&mut self, client: &Client) {
        self.relative_manager = client.bind(1);
        if self.relative_manager.is_none() {
            log::info!("wayland: no zwp_relative_pointer_manager_v1; no relative motion");
        }
        self.constraints = client.bind(1);
        if self.constraints.is_none() {
            log::info!("wayland: no zwp_pointer_constraints_v1; the pointer is never locked");
        }
    }

    /// Keep a seat event for [`Seat::deliver`]; anything else back.
    pub(super) fn take(&mut self, event: Event) -> Option<Event> {
        match event {
            // In order with the pointer's own events: a relative motion
            // comes before the pointer motion it belongs to.
            Event::Keyboard(_)
            | Event::Pointer(_)
            | Event::RelativeMotion { .. }
            | Event::Lock { .. } => {
                self.queue.push(event);
                None
            }
            other => Some(other),
        }
    }

    /// Whether seat events wait for [`Seat::deliver`].
    pub(super) fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The X window the compositor's keyboard is on.
    pub(super) fn keyboard_window(&self) -> Option<ToplevelId> {
        self.keyboard_on
    }

    /// The compositor's window the pointer is on.
    pub(super) fn pointer_surface(&self) -> Option<SurfaceId> {
        self.pointer
    }

    fn locked(&self) -> bool {
        self.lock.as_ref().is_some_and(|lock| lock.active)
    }

    /// Hand what the seat did since the last call to the core.
    pub(super) fn deliver(
        &mut self,
        client: &Client,
        work: &mut HostWork,
        windows: &HashMap<SurfaceId, (ToplevelId, u16)>,
    ) {
        self.hand_over(work, windows);
        self.ask_relative(client);
    }

    /// Turn what the seat queued into X's input, in the order it came.
    fn hand_over(&mut self, work: &mut HostWork, windows: &HashMap<SurfaceId, (ToplevelId, u16)>) {
        for event in std::mem::take(&mut self.queue) {
            match event {
                Event::Pointer(event) => self.pointer_event(work, windows, event),
                Event::Keyboard(event) => self.keyboard_event(work, windows, event),
                Event::Lock { lock, locked } => self.lock_event(&lock, locked),
                Event::RelativeMotion {
                    utime,
                    delta,
                    unaccelerated,
                } => self.relative_motion(work, utime, delta, unaccelerated),
                _ => {}
            }
        }
    }

    /// Ask for the relative pointer once the seat has a pointer.
    fn ask_relative(&mut self, client: &Client) {
        let (None, Some(manager), Some(pointer)) =
            (&self.relative, &self.relative_manager, client.pointer())
        else {
            return;
        };
        self.relative = Some(manager.get_relative_pointer(pointer, client.handle(), ()));
    }

    /// The X time of a pointer event: the server's now, as every input's.
    /// The device's stamp of the relative motion before it, if any, is
    /// only logged.
    fn motion_time(&mut self) -> u32 {
        let time = crate::clock::server_time_ms();
        if let Some(stamp) = self.moved.take() {
            log::trace!(
                "input-age stamp {stamp} xtime {time} age {}",
                monotonic_us().saturating_sub(stamp)
            );
        }
        time
    }

    fn motion(
        &mut self,
        work: &mut HostWork,
        toplevel: ToplevelId,
        local: (f64, f64),
        enter: bool,
    ) {
        let time = self.motion_time();
        let input = if enter {
            RootlessInput::PointerEnter {
                origin: ORIGIN,
                toplevel,
                local,
                time,
            }
        } else {
            RootlessInput::PointerMotionLocal {
                origin: ORIGIN,
                toplevel,
                local,
                time,
            }
        };
        work.input(HostInputEvent::Rootless(input));
    }

    fn pointer_event(
        &mut self,
        work: &mut HostWork,
        windows: &HashMap<SurfaceId, (ToplevelId, u16)>,
        event: PointerEvent,
    ) {
        match event {
            PointerEvent::Enter { surface, x, y } => {
                self.pointer = Some(surface);
                self.at = (x, y);
                match windows.get(&surface) {
                    Some(&(toplevel, crop)) => self.motion(work, toplevel, local(x, y, crop), true),
                    None => self.leave(work),
                }
            }
            PointerEvent::Leave => {
                self.pointer = None;
                self.leave(work);
            }
            // A held pointer does not move; X's moves by the relative
            // motion.
            PointerEvent::Motion { .. } if self.locked() => {}
            PointerEvent::Motion { surface, x, y } => {
                self.at = (x, y);
                if let Some(&(toplevel, crop)) = windows.get(&surface) {
                    self.motion(work, toplevel, local(x, y, crop), false);
                }
            }
            PointerEvent::Button {
                surface,
                x,
                y,
                button,
                pressed,
                ..
            } => {
                if !self.locked()
                    && (x, y) != self.at
                    && let Some(&(toplevel, crop)) = windows.get(&surface)
                {
                    self.at = (x, y);
                    self.motion(work, toplevel, local(x, y, crop), false);
                }
                let changed = if pressed {
                    self.buttons.insert(button)
                } else {
                    self.buttons.remove(&button)
                };
                if changed {
                    press(work, button, pressed);
                }
            }
            PointerEvent::Axis {
                vertical,
                horizontal,
                discrete,
                ..
            } => {
                let down = clicks(&mut self.scroll.0, vertical, discrete.0);
                let right = clicks(&mut self.scroll.1, horizontal, discrete.1);
                for (count, forward, backward) in [
                    (down, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_UP),
                    (right, SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_LEFT),
                ] {
                    let button = if count > 0 { forward } else { backward };
                    for _ in 0..count.unsigned_abs() {
                        press(work, u32::from(button), true);
                        press(work, u32::from(button), false);
                    }
                }
            }
        }
    }

    /// Off every X window: the core releases the buttons X has down and
    /// moves the pointer to the root.
    fn leave(&mut self, work: &mut HostWork) {
        self.buttons.clear();
        let time = self.motion_time();
        work.input(HostInputEvent::Rootless(RootlessInput::PointerLeave {
            origin: ORIGIN,
            time,
        }));
    }

    /// One `relative_motion`: its time is the next pointer motion's; while
    /// the lock holds, its distance moves X's pointer.
    fn relative_motion(
        &mut self,
        work: &mut HostWork,
        stamp: u64,
        delta: (f64, f64),
        unaccelerated: (f64, f64),
    ) {
        self.moved = Some(stamp);
        if !self.locked() {
            return;
        }
        let time = self.motion_time();
        work.input(HostInputEvent::Rootless(RootlessInput::PointerRelative {
            origin: ORIGIN,
            delta,
            unaccelerated,
            time,
        }));
    }

    fn keyboard_event(
        &mut self,
        work: &mut HostWork,
        windows: &HashMap<SurfaceId, (ToplevelId, u16)>,
        event: KeyboardEvent,
    ) {
        match event {
            KeyboardEvent::Enter(surface) => {
                self.keyboard_on = windows.get(&surface).map(|&(id, _)| id);
                if let Some(id) = self.keyboard_on {
                    log::info!("wayland: the keyboard is on window {id:?}");
                    work.request(HostRequest::Activated { id });
                }
            }
            KeyboardEvent::Leave => {
                self.keyboard_on = None;
                // The core lets go of every key X still has down.
                self.keys.clear();
                work.request(HostRequest::Deactivated);
            }
            KeyboardEvent::Key { code, pressed } => {
                let Some(keycode) = x_keycode(code) else {
                    log::debug!("wayland: key {code} has no X keycode");
                    return;
                };
                let changed = if pressed {
                    self.keys.insert(keycode)
                } else {
                    self.keys.remove(&keycode)
                };
                if changed {
                    work.input(HostInputEvent::Key(HostKeyEvent {
                        origin: KEY_ORIGIN,
                        pressed,
                        keycode,
                        time: crate::clock::server_time_ms(),
                        root_x: 0,
                        root_y: 0,
                        event_x: 0,
                        event_y: 0,
                        state: 0,
                    }));
                }
            }
        }
    }

    /// The compositor's word on the lock: `locked` or `unlocked`.
    fn lock_event(&mut self, object: &ZwpLockedPointerV1, active: bool) {
        let Some(lock) = self.lock.as_mut().filter(|lock| lock.object == *object) else {
            return;
        };
        if active != lock.active {
            log::info!(
                "wayland: the compositor {} the pointer on window {:?}",
                if active { "locked" } else { "unlocked" },
                lock.id
            );
        }
        lock.active = active;
    }

    /// Lock the pointer on `target`'s window (the core's decision), or let
    /// it go.
    pub(super) fn lock(&mut self, client: &Client, target: Option<(ToplevelId, SurfaceId)>) {
        if let Some(lock) = self.lock.take() {
            log::info!(
                "wayland: the pointer lock on window {:?} is let go",
                lock.id
            );
            lock.object.destroy();
        }
        let (Some((id, surface)), Some(constraints), Some(pointer)) =
            (target, &self.constraints, client.pointer())
        else {
            return;
        };
        let Some(surface) = client.surface(surface) else {
            return;
        };
        let object = constraints.lock_pointer(
            surface,
            pointer,
            None,
            zwp_pointer_constraints_v1::Lifetime::Persistent,
            client.handle(),
            (),
        );
        log::info!("wayland: asked to lock the pointer on window {id:?}");
        self.lock = Some(Lock {
            object,
            id,
            active: false,
        });
    }

    /// Window `id` is gone: a lock on it goes with it.
    pub(super) fn forget(&mut self, client: &Client, id: ToplevelId) {
        if self.keyboard_on == Some(id) {
            self.keyboard_on = None;
        }
        if self.lock.as_ref().is_some_and(|lock| lock.id == id) {
            self.lock(client, None);
        }
    }

    /// Give the compositor the X cursor in effect, or hide it.
    pub(super) fn set_cursor(&mut self, client: &mut Client, cursor: Option<&ActiveCursorImage>) {
        let key = cursor.map(|image| (image.host_xid, image.serial, (image.hot_x, image.hot_y)));
        if self.cursor == Some(key) {
            return;
        }
        self.cursor = Some(key);
        let Some(image) = cursor else {
            client.hide_cursor();
            return;
        };
        let pixels = premultiplied(&image.bgra_bytes);
        match client.set_cursor_image(
            u32::from(image.width),
            u32::from(image.height),
            (i32::from(image.hot_x), i32::from(image.hot_y)),
            &pixels,
        ) {
            Ok(()) => log::debug!(
                "wayland: the cursor is {}x{} at ({}, {}), X cursor 0x{:x}",
                image.width,
                image.height,
                image.hot_x,
                image.hot_y,
                image.host_xid
            ),
            Err(error) => {
                log::warn!("wayland: the cursor: {error}");
                client.set_default_cursor();
            }
        }
    }
}

/// A position on a native window, in its backing's pixels: the window
/// leaves `crop` of the backing's edge out.
fn local(x: f64, y: f64, crop: u16) -> (f64, f64) {
    (x + f64::from(crop), y + f64::from(crop))
}

/// Press or let go of pointer button `button` (an evdev code, or one of the
/// server's synthetic scroll codes).
fn press(work: &mut HostWork, button: u32, pressed: bool) {
    let Ok(button) = u16::try_from(button) else {
        return;
    };
    work.input(HostInputEvent::PointerButton {
        origin: ORIGIN,
        button,
        pressed,
        time: crate::clock::server_time_ms(),
    });
}

/// Whole wheel clicks from one axis event: the compositor's own count when
/// it gives one, which also forgets what was left over, else `value` added
/// to `residue`, a click every [`SCROLL_PER_CLICK`].
fn clicks(residue: &mut f64, value: f64, discrete: i32) -> i32 {
    if discrete != 0 {
        *residue = 0.0;
        return discrete;
    }
    *residue += value;
    let whole = (*residue / SCROLL_PER_CLICK).trunc();
    *residue -= whole * SCROLL_PER_CLICK;
    #[allow(clippy::cast_possible_truncation)]
    let whole = whole as i32;
    whole
}

/// An evdev key code as an X keycode, which is 8 more and at most 255.
fn x_keycode(code: u32) -> Option<u8> {
    u8::try_from(code.checked_add(8)?).ok()
}

/// A cursor's straight-alpha B, G, R, A pixels as `wl_shm`'s premultiplied
/// ARGB8888, which is B, G, R, A in memory too.
fn premultiplied(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        let alpha = u16::from(pixel[3]);
        let scale = |channel: u8| {
            #[allow(clippy::cast_possible_truncation)]
            let scaled = ((u16::from(channel) * alpha + 127) / 255) as u8;
            scaled
        };
        out.extend_from_slice(&[scale(pixel[0]), scale(pixel[1]), scale(pixel[2]), pixel[3]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rootless::display::HostItem;

    #[test]
    fn the_wheel_counts_the_compositors_clicks_or_ten_units_a_click() {
        let mut residue = 0.0;
        assert_eq!(clicks(&mut residue, 15.0, 0), 1);
        assert_eq!(clicks(&mut residue, 5.0, 0), 1);
        assert_eq!(clicks(&mut residue, 3.0, 2), 2);
        assert!(residue.abs() < f64::EPSILON);
        assert_eq!(clicks(&mut residue, -25.0, 0), -2);
    }

    /// The X time of each input the seat hands over, in order.
    fn times(work: &HostWork) -> Vec<u32> {
        work.iter()
            .filter_map(|item| match item {
                HostItem::Input(input) => Some(input),
                _ => None,
            })
            .filter_map(|input| match input {
                HostInputEvent::Key(key) => Some(key.time),
                HostInputEvent::PointerButton { time, .. } => Some(*time),
                HostInputEvent::Rootless(
                    RootlessInput::PointerEnter { time, .. }
                    | RootlessInput::PointerMotionLocal { time, .. }
                    | RootlessInput::PointerLeave { time, .. }
                    | RootlessInput::PointerRelative { time, .. },
                ) => Some(*time),
                _ => None,
            })
            .collect()
    }

    /// What a motion's own device time would break: a motion stamped by
    /// its device long before it came (a busy compositor, or a stamp on
    /// another clock) handed to X before a button pressed after it, at
    /// an earlier time than the button's and than the server's own now.
    #[test]
    fn every_inputs_time_is_the_servers_in_the_order_it_came() {
        let surface = SurfaceId::for_test(1);
        let windows = HashMap::from([(surface, (ToplevelId::fake(0x40_0001), 0))]);
        let mut seat = Seat::default();
        let before = crate::clock::server_time_ms();
        // Stamped five seconds before now, on CLOCK_MONOTONIC.
        let stale = monotonic_us().saturating_sub(5_000_000);
        seat.queue = vec![
            Event::Pointer(PointerEvent::Enter {
                surface,
                x: 1.0,
                y: 1.0,
            }),
            Event::RelativeMotion {
                utime: stale,
                delta: (4.0, 0.0),
                unaccelerated: (4.0, 0.0),
            },
            Event::Pointer(PointerEvent::Motion {
                surface,
                x: 5.0,
                y: 1.0,
            }),
            Event::Pointer(PointerEvent::Button {
                surface,
                x: 5.0,
                y: 1.0,
                button: 0x110,
                pressed: true,
            }),
            Event::Keyboard(KeyboardEvent::Key {
                code: 30,
                pressed: true,
            }),
            Event::Pointer(PointerEvent::Leave),
        ];
        let mut work = HostWork::default();
        seat.hand_over(&mut work, &windows);
        let times = times(&work);
        assert_eq!(
            times.len(),
            5,
            "enter, motion, button, key, leave: {times:?}"
        );
        assert!(
            times.windows(2).all(|pair| pair[0] <= pair[1]),
            "out of order: {times:?}"
        );
        assert!(
            times.iter().all(|&time| time >= before),
            "older than the server's now: {times:?}"
        );
    }

    /// The seat knows which window the compositor's keyboard is on: an
    /// activation the host may grant itself is for that window alone.
    #[test]
    fn the_seat_knows_the_window_the_keyboard_is_on() {
        use yserver_core::rootless::WmRequest;

        use super::super::activation_granted;

        let (on, other) = (SurfaceId::for_test(1), SurfaceId::for_test(2));
        let id = ToplevelId::fake(0x40_0001);
        let windows = HashMap::from([(on, (id, 0))]);
        let mut seat = Seat::default();
        let mut work = HostWork::default();
        seat.queue = vec![Event::Keyboard(KeyboardEvent::Enter(on))];
        seat.hand_over(&mut work, &windows);
        assert_eq!(seat.keyboard_window(), Some(id));
        assert!(activation_granted(
            WmRequest::Activate,
            seat.keyboard_window(),
            id
        ));
        assert!(!activation_granted(
            WmRequest::Activate,
            seat.keyboard_window(),
            ToplevelId::fake(0x40_0002),
        ));
        assert!(!activation_granted(
            WmRequest::Minimize,
            seat.keyboard_window(),
            id,
        ));
        // A window that is no X top-level, and then none at all.
        seat.queue = vec![Event::Keyboard(KeyboardEvent::Enter(other))];
        seat.hand_over(&mut work, &windows);
        assert_eq!(seat.keyboard_window(), None);
        seat.queue = vec![
            Event::Keyboard(KeyboardEvent::Enter(on)),
            Event::Keyboard(KeyboardEvent::Leave),
        ];
        seat.hand_over(&mut work, &windows);
        assert_eq!(seat.keyboard_window(), None);
    }

    /// The pointer's and the keys' input comes from the two slaves the host
    /// announces, never from a master (`InputOrigin::NestedHost`), whose
    /// events carry the master as their source.
    #[test]
    fn the_seats_input_comes_from_the_slaves_it_announces() {
        let [
            HostInputEvent::DeviceAdded(pointer),
            HostInputEvent::DeviceAdded(keyboard),
        ] = announcements()
        else {
            panic!("two devices");
        };
        assert!(pointer.capabilities.pointer && !pointer.capabilities.keyboard);
        assert!(keyboard.capabilities.keyboard && !keyboard.capabilities.pointer);
        assert_ne!(pointer.source_id, keyboard.source_id);
        assert_eq!(ORIGIN, InputOrigin::Physical(pointer.source_id));
        assert_eq!(KEY_ORIGIN, InputOrigin::Physical(keyboard.source_id));
    }

    #[test]
    fn an_x_keycode_is_evdev_plus_eight() {
        assert_eq!(x_keycode(30), Some(38));
        assert_eq!(x_keycode(248), None);
    }

    #[test]
    fn a_cursor_is_premultiplied() {
        assert_eq!(premultiplied(&[255, 128, 0, 128]), [128, 64, 0, 128]);
        assert_eq!(premultiplied(&[9, 9, 9, 0]), [0, 0, 0, 0]);
    }
}
