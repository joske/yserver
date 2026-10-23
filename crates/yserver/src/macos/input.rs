//! NSEvent → plain values for the OS-neutral translator, sent to the core
//! through the shell link. Every key the app receives goes to X while an X
//! window (or none) is key, except the app menu's shortcuts with key
//! equivalents on.

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    rc::Rc,
    sync::Arc,
};

use block2::RcBlock;
use objc2::{
    ClassType, MainThreadMarker, Message,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventMask, NSEventPhase, NSEventType, NSView, NSWindow,
    NSWindowDidBecomeKeyNotification,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};

use yserver_core::{core_loop::HostInputEvent, rootless::ToplevelId};

use crate::{
    rootless::native::seat::PointerTracker,
    settings::Live,
    surface::{
        input::{
            pointer::{self, RawPointer, RawScroll, ScrollPhase, Scroller, local_position},
            translate::{KeyKind, RawKey, Translator, caps_locked, key_goes_to_x},
        },
        link::{ShellEvent, ShellLink},
    },
};

// Carbon's physical keyboard layout query (HIToolbox).
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn LMGetKbdType() -> u8;
    fn KBGetLayoutType(keyboard_type: i16) -> u32;
}

/// `kKeyboardISO`, the four-char code `'ISO '`.
const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");

/// Whether the keyboard is an ISO one (the `ISO_Section` key left of `1`).
fn iso_keyboard() -> bool {
    // SAFETY: plain Carbon queries without arguments to validate.
    let layout = unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) };
    layout == KEYBOARD_ISO
}

/// The window's input state, shared by the view, the key monitor and the
/// window delegate on the main thread.
pub(super) struct ShellInput {
    link: Arc<ShellLink>,
    /// The output in X pixels, as last posted to the core; pointer
    /// positions clamp to it.
    fb: Cell<(u16, u16)>,
    translator: RefCell<Translator>,
    scroller: RefCell<Scroller>,
    /// Rootless: which top-level the pointer is in, and held buttons.
    pointer: RefCell<PointerTracker>,
    /// Rootless: the mouse-down still held, for `_NET_WM_MOVERESIZE` moves.
    held_down: RefCell<Option<(ToplevelId, Retained<NSEvent>)>>,
    /// The settings that apply immediately.
    live: Cell<Live>,
}

impl ShellInput {
    pub(super) fn new(link: Arc<ShellLink>, fb: (u16, u16), live: Live) -> Rc<Self> {
        let iso = iso_keyboard();
        log::info!(
            "macos: keyboard layout {}",
            if iso { "ISO" } else { "ANSI/JIS" }
        );
        Rc::new(Self {
            link,
            fb: Cell::new(fb),
            translator: RefCell::new(Translator::new(iso)),
            scroller: RefCell::new(Scroller::default()),
            pointer: RefCell::new(PointerTracker::default()),
            held_down: RefCell::new(None),
            live: Cell::new(live),
        })
    }

    /// The settings that apply immediately.
    pub(super) fn live(&self) -> Live {
        self.live.get()
    }

    /// The Settings window changed a live setting.
    pub(super) fn set_live(&self, live: Live) {
        self.live.set(live);
    }

    /// The window now shows a `fb`-sized output.
    pub(super) fn set_fb(&self, fb: (u16, u16)) {
        self.fb.set(fb);
    }

    /// Route every key event of the app to X, AppKit sees none of them;
    /// except while a modal alert or another non-X window is key, and the
    /// app menu's shortcuts with key equivalents on.
    pub(super) fn install_key_monitor(
        self: &Rc<Self>,
        mtm: MainThreadMarker,
    ) -> Option<Retained<AnyObject>> {
        let input = Rc::clone(self);
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            let app = NSApplication::sharedApplication(mtm);
            if app.modalWindow().is_some() || app.keyWindow().is_some_and(|w| !is_x_window(&w)) {
                return event.as_ptr();
            }
            // SAFETY: AppKit hands the monitor a live event.
            let event = unsafe { event.as_ref() };
            let Some(raw) = raw_key(event) else {
                return std::ptr::null_mut();
            };
            // The menu runs the item itself (Quit may ask in a modal alert).
            let menu = || {
                app.mainMenu()
                    .is_some_and(|menu| menu.performKeyEquivalent(event))
            };
            if key_goes_to_x(&raw, input.live.get().key_equivalents, menu) {
                input.key(raw);
            }
            std::ptr::null_mut()
        });
        let mask = NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged;
        // SAFETY: the handler returns the event or null, which drops it.
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
    }

    /// Release in X every key and button held when a window of ours that
    /// is not an X window (an alert) becomes key, so none sticks.
    pub(super) fn install_focus_observer(
        self: &Rc<Self>,
    ) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
        let input = Rc::clone(self);
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: the center hands the block a live notification.
            let window = unsafe { notification.as_ref() }
                .object()
                .and_then(|object| object.downcast::<NSWindow>().ok());
            if window.is_some_and(|window| !is_x_window(&window)) {
                log::debug!("macos: a non-X window became key");
                input.release_all();
            }
        });
        // SAFETY: window notifications post on the main thread, which owns
        // the block's `Rc`; without a queue it runs there, synchronously.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidBecomeKeyNotification),
                None,
                None,
                &block,
            )
        }
    }

    fn key(&self, raw: RawKey) {
        log::debug!(
            "macos key: kvk={:#04x} {:?} flags={:#x} repeat={}",
            raw.kvk,
            raw.kind,
            raw.flags,
            raw.is_repeat
        );
        let mut translator = self.translator.borrow_mut();
        self.link
            .send_input(|time, out| translator.key(raw, time, out));
    }

    /// The pointer moved over (or dragged across) `view`.
    pub(super) fn motion(&self, view: &NSView, event: &NSEvent) {
        let raw = raw_pointer(view, event);
        log::trace!("macos motion: {raw:?}");
        let fb = self.fb.get();
        self.link
            .send_input(|time, out| out.push(pointer::motion(&raw, fb, time)));
    }

    /// A mouse button changed over `view`; the position goes first.
    pub(super) fn button(&self, view: &NSView, event: &NSEvent, pressed: bool) {
        let raw = raw_pointer(view, event);
        let fb = self.fb.get();
        let number = event.buttonNumber() as i64;
        let flags = event.modifierFlags().0 as u64;
        let emulate = self.live.get().emulate_buttons;
        log::debug!("macos button {number} pressed={pressed}: {raw:?}");
        let mut translator = self.translator.borrow_mut();
        self.link.send_input(|time, out| {
            out.push(pointer::motion(&raw, fb, time));
            translator.button(number, pressed, flags, emulate, time, out);
        });
    }

    /// Rootless: the pointer moved over (or dragged across) top-level `id`'s
    /// `view`, at `scale` X pixels per point (the global scale, D2).
    pub(super) fn motion_local(&self, id: ToplevelId, view: &NSView, event: &NSEvent, scale: f64) {
        let local = local_position(&raw_pointer_at(view, event, scale));
        log::trace!("macos motion {id:?}: {local:?}");
        let mut pointer = self.pointer.borrow_mut();
        self.link.send_input(|time, out| {
            out.push(HostInputEvent::Rootless(pointer.moved(id, local, time)));
        });
    }

    /// Rootless: the pointer entered `id`'s view (back into it during a
    /// drag cancels the deferred leave).
    pub(super) fn entered_local(&self, id: ToplevelId, view: &NSView, event: &NSEvent, scale: f64) {
        let local = local_position(&raw_pointer_at(view, event, scale));
        let mut pointer = self.pointer.borrow_mut();
        self.link.send_input(|time, out| {
            out.push(HostInputEvent::Rootless(pointer.entered(id, local, time)));
        });
    }

    /// Rootless: a button changed over `id`; the position goes first, a
    /// leave deferred while it was held goes last.
    /// `under`: during a drag, the top-level under the pointer and the
    /// position in it (see [`Self::dragged_local`]).
    pub(super) fn button_local(
        &self,
        id: ToplevelId,
        view: &NSView,
        event: &NSEvent,
        pressed: bool,
        scale: f64,
        under: Option<(Option<ToplevelId>, (f64, f64))>,
    ) {
        let local = local_position(&raw_pointer_at(view, event, scale));
        *self.held_down.borrow_mut() = pressed.then(|| (id, event.retain()));
        let number = event.buttonNumber() as i64;
        let flags = event.modifierFlags().0 as u64;
        let emulate = self.live.get().emulate_buttons;
        log::debug!("macos button {number} pressed={pressed} {id:?}: {local:?}");
        let mut pointer = self.pointer.borrow_mut();
        let mut translator = self.translator.borrow_mut();
        self.link.send_input(|time, out| {
            let at = match under {
                Some((under, local)) => pointer.dragged(under, local, time),
                None => Some(pointer.moved(id, local, time)),
            };
            out.extend(at.map(HostInputEvent::Rootless));
            translator.button(number, pressed, flags, emulate, time, out);
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

    /// Rootless: the mouse-down on `id` whose button is still held.
    pub(super) fn held_down(&self, id: ToplevelId) -> Option<Retained<NSEvent>> {
        self.held_down
            .borrow()
            .as_ref()
            .filter(|(on, _)| *on == id)
            .map(|(_, event)| event.clone())
    }

    /// Rootless: the pointer is in one of our top-levels.
    pub(super) fn pointer_inside(&self) -> bool {
        self.pointer.borrow().inside().is_some()
    }

    /// Rootless: the pointer left `id`'s view.
    pub(super) fn exited_local(&self, id: ToplevelId) {
        let mut pointer = self.pointer.borrow_mut();
        self.link.send_input(|time, out| {
            if let Some(leave) = pointer.exited(id, time) {
                out.push(HostInputEvent::Rootless(leave));
            }
        });
    }

    pub(super) fn scroll(&self, event: &NSEvent) {
        let raw = RawScroll {
            dx: event.scrollingDeltaX(),
            dy: event.scrollingDeltaY(),
            precise: event.hasPreciseScrollingDeltas(),
            phase: scroll_phase(event.phase()),
            momentum: scroll_phase(event.momentumPhase()),
        };
        log::trace!("macos scroll: {raw:?}");
        let mut scroller = self.scroller.borrow_mut();
        self.link
            .send_input(|time, out| scroller.scroll(raw, time, out));
    }

    /// The window resigned key: release everything held.
    pub(super) fn focus_lost(&self) {
        log::debug!("macos: window resigned key");
        self.release_all();
    }

    fn release_all(&self) {
        let mut translator = self.translator.borrow_mut();
        self.link
            .send_input(|time, out| translator.release_all(time, out));
    }

    /// The window became key: let the core match the Caps Lock state.
    pub(super) fn focus_gained(&self) {
        let flags = NSEvent::modifierFlags_class().0 as u64;
        log::debug!("macos: window became key, flags={flags:#x}");
        self.link.post(ShellEvent::LockState {
            caps: caps_locked(flags),
        });
    }
}

/// A key event's plain values.
fn raw_key(event: &NSEvent) -> Option<RawKey> {
    let kind = match event.r#type() {
        NSEventType::KeyDown => KeyKind::Down,
        NSEventType::KeyUp => KeyKind::Up,
        NSEventType::FlagsChanged => KeyKind::FlagsChanged,
        _ => return None,
    };
    Some(RawKey {
        kvk: event.keyCode(),
        kind,
        flags: event.modifierFlags().0 as u64,
        // `isARepeat` raises on a flagsChanged event.
        is_repeat: kind != KeyKind::FlagsChanged && event.isARepeat(),
    })
}

/// Whether `window` shows X: the root (rooted) or a top-level (rootless).
fn is_x_window(window: &NSWindow) -> bool {
    window.contentView().is_some_and(|view| {
        view.isKindOfClass(super::view::YserverView::class())
            || super::rootless::is_toplevel_view(&view)
    })
}

fn raw_pointer(view: &NSView, event: &NSEvent) -> RawPointer {
    let scale = view
        .window()
        .map_or(1.0, |window| super::scale::xpp(&window));
    raw_pointer_at(view, event, scale)
}

fn raw_pointer_at(view: &NSView, event: &NSEvent, scale: f64) -> RawPointer {
    let at = view.convertPoint_fromView(event.locationInWindow(), None);
    RawPointer {
        x_pt: at.x,
        y_pt: at.y,
        view_h_pt: view.bounds().size.height,
        scale,
        dx_pt: event.deltaX(),
        dy_pt: event.deltaY(),
    }
}

fn scroll_phase(phase: NSEventPhase) -> ScrollPhase {
    if phase == NSEventPhase::None {
        ScrollPhase::None
    } else if phase.contains(NSEventPhase::Ended) {
        ScrollPhase::Ended
    } else if phase.contains(NSEventPhase::Cancelled) {
        ScrollPhase::Cancelled
    } else if phase.contains(NSEventPhase::Began) {
        ScrollPhase::Began
    } else if phase.contains(NSEventPhase::Changed) {
        ScrollPhase::Changed
    } else {
        ScrollPhase::Other
    }
}
