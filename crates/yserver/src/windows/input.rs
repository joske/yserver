//! Win32 keyboard and mouse messages → plain values for the OS-neutral
//! translator, sent to the core through the shell link. Keys go by scan
//! code, so the X keymap applies whatever the Windows layout is; every key
//! the window receives goes to X, Alt and F10 included.

use std::{
    cell::{Cell, RefCell},
    sync::Arc,
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::SystemServices::{MK_LBUTTON, MK_MBUTTON, MK_RBUTTON, MK_XBUTTON1, MK_XBUTTON2},
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyState, ReleaseCapture, SetCapture, VK_CAPITAL, VK_CONTROL, VK_MENU, VK_NUMLOCK,
            VK_PAUSE, VK_SNAPSHOT,
        },
        WindowsAndMessaging::{
            MSG, PM_NOREMOVE, PeekMessageW, WM_KEYDOWN, WM_KEYFIRST, WM_KEYLAST, WM_SYSKEYDOWN,
            XBUTTON1, XBUTTON2,
        },
    },
};

use yserver_core::{core_loop::HostInputEvent, rootless::ToplevelId};

use crate::{
    rootless::native::seat::PointerTracker,
    surface::{
        input::{
            keymap::ev,
            pointer::{self, RawPointer, RawScroll, ScrollPhase, Scroller},
            scancode::{self, EXTENDED},
            translate::Translator,
        },
        link::{ShellEvent, ShellLink},
    },
};

/// `WHEEL_DELTA`: one wheel notch.
const WHEEL_DELTA: f64 = 120.0;
/// Linux `KEY_PAUSE`, `KEY_SYSRQ` (`input-event-codes.h`).
const KEY_PAUSE: u16 = 119;
const KEY_SYSRQ: u16 = 99;

/// What a `WM_KEYDOWN`/`WM_KEYUP` (or `WM_SYS*`) carries.
#[derive(Debug, Clone, Copy)]
pub(super) struct RawKeyMessage {
    pub(super) vk: u16,
    /// The set-1 scan code, `EXTENDED` set for an `E0` key.
    pub(super) scancode: u16,
    pub(super) pressed: bool,
    /// The key was already down: the window system's autorepeat.
    pub(super) repeat: bool,
    /// `GetMessageTime`, to pair the left Ctrl that AltGr fakes.
    pub(super) time: u32,
}

impl RawKeyMessage {
    pub(super) fn new(wparam: WPARAM, lparam: LPARAM, pressed: bool, time: u32) -> Self {
        // lParam: bits 16–23 the scan code, 24 the extended flag, 30 the
        // previous key state.
        let bits = lparam as u32;
        let code = u16::try_from((bits >> 16) & 0xFF).unwrap_or(0);
        Self {
            vk: u16::try_from(wparam & 0xFFFF).unwrap_or(0),
            scancode: if bits & (1 << 24) != 0 {
                EXTENDED | code
            } else {
                code
            },
            pressed,
            repeat: pressed && bits & (1 << 30) != 0,
            time,
        }
    }

    /// The evdev code of the key: by virtual key for the three whose scan
    /// code Windows folds (see [`scancode`]), else by scan code.
    fn evdev(self) -> Option<u16> {
        match self.vk {
            VK_NUMLOCK => Some(ev::KEY_NUMLOCK),
            VK_PAUSE => Some(KEY_PAUSE),
            VK_SNAPSHOT => Some(KEY_SYSRQ),
            _ => scancode::evdev_code(self.scancode),
        }
    }
}

/// The window's input state, on the UI thread.
pub(super) struct ShellInput {
    link: Arc<ShellLink>,
    /// The output in X pixels, as last posted to the core; pointer
    /// positions clamp to it.
    fb: Cell<(u16, u16)>,
    /// Device pixels per X pixel per axis.
    factor: Cell<u32>,
    translator: RefCell<Translator>,
    scroller: RefCell<Scroller>,
    /// The last client position, for motion deltas.
    last: Cell<Option<(i32, i32)>>,
    /// Rootless: which top-level the pointer is in.
    pointer: RefCell<PointerTracker>,
}

impl ShellInput {
    pub(super) fn new(link: Arc<ShellLink>, fb: (u16, u16)) -> Self {
        Self {
            link,
            fb: Cell::new(fb),
            factor: Cell::new(1),
            translator: RefCell::new(Translator::default()),
            scroller: RefCell::new(Scroller::default()),
            last: Cell::new(None),
            pointer: RefCell::new(PointerTracker::default()),
        }
    }

    /// The window now shows a `fb`-sized output.
    pub(super) fn set_fb(&self, fb: (u16, u16)) {
        self.fb.set(fb);
    }

    /// One X pixel covers `factor`x`factor` device pixels from now on.
    pub(super) fn set_factor(&self, factor: u32) {
        self.factor.set(factor.max(1));
    }

    /// A position in device pixels (a client or screen position, or an
    /// offset) in X pixels, unfloored.
    pub(super) fn to_x(&self, device: (i32, i32)) -> (f64, f64) {
        let factor = f64::from(self.factor.get());
        (f64::from(device.0) / factor, f64::from(device.1) / factor)
    }

    /// A key message for `hwnd`.
    pub(super) fn key(&self, hwnd: HWND, raw: RawKeyMessage) {
        log::debug!(
            "windows key: vk={:#04x} scan={:#06x} pressed={} repeat={}",
            raw.vk,
            raw.scancode,
            raw.pressed,
            raw.repeat
        );
        // The core repeats held keys itself, at the XKB rate.
        if raw.repeat || is_altgr_control(hwnd, raw) {
            return;
        }
        let Some(evdev) = raw.evdev() else {
            return;
        };
        let mut translator = self.translator.borrow_mut();
        self.link
            .send_input(|time, out| translator.evdev_key(evdev, raw.pressed, time, out));
    }

    /// The pointer moved to client device pixel `at`.
    pub(super) fn motion(&self, at: (i32, i32)) {
        let raw = self.raw_pointer(at);
        log::trace!("windows motion: {raw:?}");
        let fb = self.fb.get();
        self.link
            .send_input(|time, out| out.push(pointer::motion(&raw, fb, time)));
    }

    /// Mouse button `number` (0 left, 1 right, 2 middle, 3 back, 4 forward)
    /// changed at client device pixel `at`; the position goes first. The window
    /// keeps the mouse while any button is held (`held`: the `MK_*` flags
    /// after the message).
    pub(super) fn button(
        &self,
        hwnd: HWND,
        number: i64,
        pressed: bool,
        at: (i32, i32),
        held: usize,
    ) {
        let raw = self.raw_pointer(at);
        let fb = self.fb.get();
        log::debug!("windows button {number} pressed={pressed}: {raw:?}");
        capture(hwnd, pressed, held);
        let mut translator = self.translator.borrow_mut();
        self.link.send_input(|time, out| {
            out.push(pointer::motion(&raw, fb, time));
            translator.button(number, pressed, 0, false, time, out);
        });
    }

    /// Rootless: the pointer moved over top-level `id`, at X pixel `local`
    /// of it.
    pub(super) fn motion_local(&self, id: ToplevelId, local: (f64, f64)) {
        log::trace!("windows motion {id:?}: {local:?}");
        let mut pointer = self.pointer.borrow_mut();
        self.link.send_input(|time, out| {
            out.push(HostInputEvent::Rootless(pointer.moved(id, local, time)));
        });
    }

    /// Rootless: a button changed over `hwnd`, top-level `id`, at `local`;
    /// the position goes first, a leave deferred while it was held goes
    /// last. `under`: during a drag, the top-level under the pointer and the
    /// position in it (see [`Self::dragged_local`]).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn button_local(
        &self,
        hwnd: HWND,
        id: ToplevelId,
        number: i64,
        pressed: bool,
        local: (f64, f64),
        held: usize,
        under: Option<(Option<ToplevelId>, (f64, f64))>,
    ) {
        log::debug!("windows button {number} pressed={pressed} {id:?}: {local:?}");
        capture(hwnd, pressed, held);
        let mut pointer = self.pointer.borrow_mut();
        let mut translator = self.translator.borrow_mut();
        self.link.send_input(|time, out| {
            let at = match under {
                Some((under, local)) => pointer.dragged(under, local, time),
                None => Some(pointer.moved(id, local, time)),
            };
            out.extend(at.map(HostInputEvent::Rootless));
            translator.button(number, pressed, 0, false, time, out);
            if let Some(leave) = pointer.button(pressed, time) {
                out.push(HostInputEvent::Rootless(leave));
            }
        });
    }

    /// Rootless: drag motion at the top-level under the pointer (`under`,
    /// `local` in it), or relative to the current one over none of ours.
    pub(super) fn dragged_local(&self, under: Option<ToplevelId>, local: (f64, f64)) {
        let mut pointer = self.pointer.borrow_mut();
        self.link.send_input(|time, out| {
            out.extend(
                pointer
                    .dragged(under, local, time)
                    .map(HostInputEvent::Rootless),
            );
        });
    }

    /// Rootless: a button is held, and the top-level the pointer is in.
    pub(super) fn drag_state(&self) -> (bool, Option<ToplevelId>) {
        let pointer = self.pointer.borrow();
        (pointer.dragging(), pointer.inside())
    }

    /// Rootless: the pointer is in one of our top-levels.
    pub(super) fn pointer_inside(&self) -> bool {
        self.pointer.borrow().inside().is_some()
    }

    /// Rootless: the pointer left `id`'s client area.
    pub(super) fn exited_local(&self, id: ToplevelId) {
        let mut pointer = self.pointer.borrow_mut();
        self.link.send_input(|time, out| {
            if let Some(leave) = pointer.exited(id, time) {
                out.push(HostInputEvent::Rootless(leave));
            }
        });
    }

    /// Rootless: a move or size loop takes the mouse.
    pub(super) fn release_capture(&self) {
        // SAFETY: a plain call on this thread.
        unsafe { ReleaseCapture() };
    }

    /// A wheel turned by `delta` (`WHEEL_DELTA` per notch), `horizontal`
    /// for `WM_MOUSEHWHEEL`.
    pub(super) fn wheel(&self, delta: i16, horizontal: bool) {
        let notches = f64::from(delta) / WHEEL_DELTA;
        // Positive: away from the user (up), or tilted right; `RawScroll`'s
        // positive `dx` is left.
        let (dx, dy) = if horizontal {
            (-notches, 0.0)
        } else {
            (0.0, notches)
        };
        let raw = RawScroll {
            dx,
            dy,
            precise: false,
            phase: ScrollPhase::None,
            momentum: ScrollPhase::None,
        };
        log::trace!("windows scroll: {raw:?}");
        let mut scroller = self.scroller.borrow_mut();
        self.link
            .send_input(|time, out| scroller.scroll(raw, time, out));
    }

    /// The window lost the keyboard: release everything held.
    pub(super) fn focus_lost(&self) {
        log::debug!("windows: window lost focus");
        let mut translator = self.translator.borrow_mut();
        self.link
            .send_input(|time, out| translator.release_all(time, out));
    }

    /// The window got the keyboard: let the core match the Caps Lock state.
    pub(super) fn focus_gained(&self) {
        // SAFETY: a plain query; the low bit is the toggle state.
        let caps = unsafe { GetKeyState(i32::from(VK_CAPITAL)) } & 1 != 0;
        log::debug!("windows: window got focus, caps={caps}");
        self.link.post(ShellEvent::LockState { caps });
    }

    /// Client device pixel `at` as the translator's view points: one point
    /// per X pixel, the origin flipped to the bottom of the output.
    fn raw_pointer(&self, at: (i32, i32)) -> RawPointer {
        let (dx, dy) = self
            .last
            .replace(Some(at))
            .map_or((0, 0), |(x, y)| (at.0 - x, at.1 - y));
        let height = f64::from(self.fb.get().1);
        let (x, y) = self.to_x(at);
        let (dx, dy) = self.to_x((dx, dy));
        RawPointer {
            x_pt: x,
            y_pt: height - y,
            view_h_pt: height,
            scale: 1.0,
            dx_pt: dx,
            dy_pt: dy,
        }
    }
}

/// Keep the mouse in `hwnd` while any button is held (`held`: the `MK_*`
/// flags after a button message).
fn capture(hwnd: HWND, pressed: bool, held: usize) {
    let any_held =
        held & (MK_LBUTTON | MK_RBUTTON | MK_MBUTTON | MK_XBUTTON1 | MK_XBUTTON2) as usize != 0;
    // SAFETY: a live window of this thread.
    unsafe {
        if pressed {
            SetCapture(hwnd);
        } else if !any_held {
            ReleaseCapture();
        }
    }
}

/// Whether `raw` is the left Ctrl press Windows sends ahead of a right Alt
/// press with an AltGr layout: the next key message is that right Alt
/// press, at the same time. Its release then finds no key down and is
/// dropped.
fn is_altgr_control(hwnd: HWND, raw: RawKeyMessage) -> bool {
    if !raw.pressed || raw.vk != VK_CONTROL || raw.scancode != 0x1D {
        return false;
    }
    // SAFETY: zeroed is a valid MSG.
    let mut next: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: a live window of this thread; writes `next` and leaves the
    // queue as it is.
    if unsafe { PeekMessageW(&raw mut next, hwnd, WM_KEYFIRST, WM_KEYLAST, PM_NOREMOVE) } == 0
        || !matches!(next.message, WM_KEYDOWN | WM_SYSKEYDOWN)
    {
        return false;
    }
    let next = RawKeyMessage::new(next.wParam, next.lParam, true, next.time);
    next.vk == VK_MENU && next.scancode == EXTENDED | 0x38 && next.time == raw.time
}

/// The 0-based button number of an `XBUTTON*` message's `wParam`.
pub(super) fn x_button(wparam: WPARAM) -> Option<i64> {
    match u16::try_from((wparam >> 16) & 0xFFFF).unwrap_or(0) {
        XBUTTON1 => Some(3),
        XBUTTON2 => Some(4),
        _ => None,
    }
}

/// The client position of a mouse message's `lParam`.
pub(super) fn client_position(lparam: LPARAM) -> (i32, i32) {
    // Signed: a captured pointer outside the window goes negative.
    let x = i32::from((lparam & 0xFFFF) as u16 as i16);
    let y = i32::from(((lparam >> 16) & 0xFFFF) as u16 as i16);
    (x, y)
}
