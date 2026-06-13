//! Sender-only libinput thread for the single-threaded core.
//!
//! The thread owns the `SendContext` and an `epoll` set wrapping the
//! libinput fd. Each batch of `crate::input::InputEvent`s gets mapped
//! to `HostInputEvent`s and pushed onto the core's message channel.
//! Consecutive `PointerMotion` events are coalesced — at most one
//! motion stays in flight to the core at any given moment, and the
//! latest position wins. Buttons and keys are never coalesced and
//! flush any pending motion immediately.
//!
//! Cursor accumulation lives on this thread (relative deltas + clamped
//! absolute mappings). The backend keeps its own cursor mirror updated
//! when it receives `HostInputEvent::PointerMotion`. A brief skew is
//! tolerable.
//!
//! Spec: `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`
//! Plan: `docs/superpowers/plans/2026-05-06-single-threaded-core.md` §E2.

use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    sync::atomic::{AtomicU8, Ordering},
};

use nix::sys::{
    epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout},
    eventfd::{EfdFlags, EventFd},
};
use yserver_core::{
    core_loop::{
        CoreSender, HostInputEvent, Message, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT,
        SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_UP,
    },
    host_x11::HostKeyEvent,
};

use crate::input::{
    InputEvent, SendContext,
    hotkey::{Hotkey, HotkeyDetector},
};

/// Cursor accumulator + framebuffer dimensions held on the libinput
/// thread.
#[derive(Debug, Clone, Copy)]
pub struct LibinputThreadState {
    cursor_x: f64,
    cursor_y: f64,
    fb_w: u32,
    fb_h: u32,
    /// Hotkey detector. Tracks modifier-key state off the kernel evdev
    /// codes rather than on the X side because a grabbing client or a
    /// remapped keymap could silently consume modifier presses —
    /// hotkeys need to fire even when X dispatch is wedged.
    hotkey: HotkeyDetector,
    /// Sub-click scroll accumulators in v120 units. libinput's high-
    /// resolution wheel and finger/continuous scroll arrive as small
    /// v120 deltas that may not add up to a full 120-unit click in one
    /// event. We bank the remainder here and emit a button-4/5/6/7
    /// press+release pair each time the absolute accumulator crosses
    /// 120. Sign convention matches libinput: positive Y = scroll down,
    /// positive X = scroll right.
    scroll_accum_x_v120: i32,
    scroll_accum_y_v120: i32,
}

impl LibinputThreadState {
    #[must_use]
    pub fn new(fb_w: u32, fb_h: u32) -> Self {
        Self {
            cursor_x: f64::from(fb_w) / 2.0,
            cursor_y: f64::from(fb_h) / 2.0,
            fb_w,
            fb_h,
            hotkey: HotkeyDetector::new(),
            scroll_accum_x_v120: 0,
            scroll_accum_y_v120: 0,
        }
    }

    #[must_use]
    pub fn cursor(&self) -> (f64, f64) {
        (self.cursor_x, self.cursor_y)
    }

    /// Translate one libinput event into a `HostInputEvent`.
    ///
    /// `time_ms` lets tests pin the timestamp; production callers pass
    /// the wall clock.
    pub(crate) fn map(&mut self, ev: InputEvent, time_ms: u32) -> HostInputEvent {
        match ev {
            InputEvent::KeyPress { keycode } => HostInputEvent::Key(HostKeyEvent {
                pressed: true,
                keycode: ((keycode + 8) & 0xff) as u8,
                time: time_ms,
                root_x: self.cursor_x as i16,
                root_y: self.cursor_y as i16,
                event_x: self.cursor_x as i16,
                event_y: self.cursor_y as i16,
                state: 0,
            }),
            InputEvent::KeyRelease { keycode } => HostInputEvent::Key(HostKeyEvent {
                pressed: false,
                keycode: ((keycode + 8) & 0xff) as u8,
                time: time_ms,
                root_x: self.cursor_x as i16,
                root_y: self.cursor_y as i16,
                event_x: self.cursor_x as i16,
                event_y: self.cursor_y as i16,
                state: 0,
            }),
            InputEvent::PointerMotion { dx, dy } => {
                self.cursor_x =
                    (self.cursor_x + dx).clamp(0.0, f64::from(self.fb_w).max(1.0) - 1.0);
                self.cursor_y =
                    (self.cursor_y + dy).clamp(0.0, f64::from(self.fb_h).max(1.0) - 1.0);
                HostInputEvent::PointerMotion {
                    x: self.cursor_x as i32,
                    y: self.cursor_y as i32,
                    time: time_ms,
                }
            }
            InputEvent::PointerMotionAbsolute { x_norm, y_norm } => {
                self.cursor_x = x_norm.clamp(0.0, 1.0) * (f64::from(self.fb_w).max(1.0) - 1.0);
                self.cursor_y = y_norm.clamp(0.0, 1.0) * (f64::from(self.fb_h).max(1.0) - 1.0);
                HostInputEvent::PointerMotion {
                    x: self.cursor_x as i32,
                    y: self.cursor_y as i32,
                    time: time_ms,
                }
            }
            InputEvent::Button { code, pressed } => HostInputEvent::PointerButton {
                button: u16::try_from(code).unwrap_or(u16::MAX),
                pressed,
                time: time_ms,
            },
            // PointerScroll is fanned out separately via `drain_scroll`
            // because it can map to N (≥ 0) press+release pairs depending
            // on accumulated v120. Reaching here means a caller forgot
            // to route it; map to a no-op-ish placeholder.
            InputEvent::PointerScroll { .. } => HostInputEvent::PointerButton {
                button: u16::MAX,
                pressed: false,
                time: time_ms,
            },
            // DeviceAdded/Removed are forwarded by process_batch before
            // reaching map(); reaching here is a routing bug, so fail loud.
            InputEvent::DeviceAdded(_) | InputEvent::DeviceRemoved { .. } => {
                unreachable!("device events are forwarded before map(); must not reach map()")
            }
        }
    }

    /// One v120 click step. libinput emits high-resolution wheel deltas
    /// in 120ths of a logical click; we accumulate fractional deltas and
    /// fire a button press+release each time |accum| crosses this.
    const V120_PER_CLICK: i32 = 120;

    /// Accumulate a scroll delta and emit press+release pairs for any
    /// completed clicks. `dy_v120 > 0` → scroll-down (button 5);
    /// `dy_v120 < 0` → scroll-up (button 4). Horizontal axis maps to
    /// button 6 (left) / 7 (right). Mixed-axis events emit Y clicks
    /// first then X clicks within a single call.
    pub(crate) fn drain_scroll(
        &mut self,
        dx_v120: i32,
        dy_v120: i32,
        time_ms: u32,
        out: &mut Vec<HostInputEvent>,
    ) {
        self.scroll_accum_x_v120 = self.scroll_accum_x_v120.saturating_add(dx_v120);
        self.scroll_accum_y_v120 = self.scroll_accum_y_v120.saturating_add(dy_v120);

        // Vertical first (more common; matches X11 button-4/5 priority).
        while self.scroll_accum_y_v120 >= Self::V120_PER_CLICK {
            self.scroll_accum_y_v120 -= Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_DOWN, time_ms);
        }
        while self.scroll_accum_y_v120 <= -Self::V120_PER_CLICK {
            self.scroll_accum_y_v120 += Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_UP, time_ms);
        }
        while self.scroll_accum_x_v120 >= Self::V120_PER_CLICK {
            self.scroll_accum_x_v120 -= Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_RIGHT, time_ms);
        }
        while self.scroll_accum_x_v120 <= -Self::V120_PER_CLICK {
            self.scroll_accum_x_v120 += Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_LEFT, time_ms);
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputThreadCommand {
    Pause = 1,
    Resume = 2,
}

impl InputThreadCommand {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Pause),
            2 => Some(Self::Resume),
            _ => None,
        }
    }
}

/// Direct-mode input-thread control channel.
#[derive(Debug)]
pub(crate) struct InputThreadControl {
    command: AtomicU8,
    efd: EventFd,
}

impl InputThreadControl {
    pub(crate) fn new() -> io::Result<Self> {
        let efd = EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
            .map_err(|e| io::Error::other(format!("InputThreadControl eventfd: {e}")))?;
        Ok(Self {
            command: AtomicU8::new(0),
            efd,
        })
    }

    pub(crate) fn pause(&self) {
        self.publish(InputThreadCommand::Pause);
    }

    pub(crate) fn resume(&self) {
        self.publish(InputThreadCommand::Resume);
    }

    pub(crate) fn fd(&self) -> std::os::fd::RawFd {
        self.efd.as_fd().as_raw_fd()
    }

    pub(crate) fn drain(&self) -> Option<InputThreadCommand> {
        let _ = self.efd.read();
        InputThreadCommand::from_raw(self.command.swap(0, Ordering::AcqRel))
    }

    fn publish(&self, command: InputThreadCommand) {
        self.command.store(command as u8, Ordering::Release);
        loop {
            match self.efd.write(1) {
                Ok(_) => break,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    log::warn!("InputThreadControl: eventfd wakeup write failed: {e}");
                    break;
                }
            }
        }
    }
}

fn push_button_click(out: &mut Vec<HostInputEvent>, button: u16, time_ms: u32) {
    out.push(HostInputEvent::PointerButton {
        button,
        pressed: true,
        time: time_ms,
    });
    out.push(HostInputEvent::PointerButton {
        button,
        pressed: false,
        time: time_ms,
    });
}

/// Process one batch of libinput events with motion coalescing.
///
/// Across the batch (and the carry-over from a previous batch via
/// `pending_motion`), at most one `PointerMotion` remains queued for
/// the core. Any non-motion event flushes the pending motion before
/// being sent. The caller drains `pending_motion` between batches if
/// it wants the core to see end-of-burst movement before the next
/// `epoll_wait`.
///
/// Per the plan (§E2), this is the function under test:
/// feeding `[Motion, Motion, Motion, Button, Motion, Motion, Motion]`
/// must produce three sender messages — `Motion(latest), Button,
/// Motion(latest)` — not seven.
pub fn process_batch(
    state: &mut LibinputThreadState,
    sender: &CoreSender,
    pending_motion: &mut Option<HostInputEvent>,
    events: impl IntoIterator<Item = InputEvent>,
    time_ms: u32,
) -> io::Result<()> {
    let mut scroll_buf: Vec<HostInputEvent> = Vec::new();
    for raw in events {
        match state.hotkey.check(&raw) {
            Some(Hotkey::Zap) => {
                // Drop any pending motion + the Backspace event itself —
                // the server is shutting down, no client should see them.
                *pending_motion = None;
                log::warn!("yserver: Ctrl-Alt-Backspace pressed — requesting shutdown (zap)");
                sender.send(Message::Shutdown)?;
                return Ok(());
            }
            Some(Hotkey::DumpScanout) => {
                // Flush any queued motion so the input stream stays
                // ordered, drop the Enter keypress itself, and ask the
                // core to dump the scanout (same code path as SIGUSR1).
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                log::info!("yserver: Ctrl-Alt-Enter pressed — dumping scanout");
                sender.send(Message::DumpScanout)?;
                continue;
            }
            Some(Hotkey::DumpDrawables) => {
                // Mirror DumpScanout: flush queued motion, drop the
                // D keypress itself, ask the core to dump
                // per-drawable storage (same code path as SIGUSR2).
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                log::info!("yserver: Ctrl-Alt-D pressed — dumping drawables");
                sender.send(Message::DumpDrawables)?;
                continue;
            }
            Some(Hotkey::SwitchVt(vt)) => {
                // Flush queued motion, drop the F-key press itself, and ask
                // the core thread to initiate the switch via VT_ACTIVATE.
                // (We can't ioctl the VT fd from here — the ConsoleGuard
                // lives on the backend/core thread.) The kernel won't switch
                // on its own because we hold the keyboard in K_OFF, so the
                // server must request it, exactly like Xorg's xf86_vt_switch.
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                log::info!("yserver: Ctrl-Alt-F{vt} pressed — requesting VT switch");
                sender.send(Message::SwitchVt(vt))?;
                continue;
            }
            None => {}
        }
        // Device add/remove are forwarded directly — they carry their
        // own data and bypass the motion/scroll mapping entirely.
        // Flush any pending motion first to preserve chronological order.
        if let InputEvent::DeviceAdded(info) = raw {
            if let Some(m) = pending_motion.take() {
                sender.send(Message::HostInput(m))?;
            }
            sender.send(Message::HostInput(HostInputEvent::DeviceAdded(info)))?;
            continue;
        }
        if let InputEvent::DeviceRemoved { device_node } = raw {
            if let Some(m) = pending_motion.take() {
                sender.send(Message::HostInput(m))?;
            }
            sender.send(Message::HostInput(HostInputEvent::DeviceRemoved {
                device_node,
            }))?;
            continue;
        }
        // Scroll fans out separately because one InputEvent may map to
        // zero or many press+release pairs depending on accumulated v120.
        if let InputEvent::PointerScroll { dx_v120, dy_v120 } = raw {
            scroll_buf.clear();
            state.drain_scroll(dx_v120, dy_v120, time_ms, &mut scroll_buf);
            if !scroll_buf.is_empty() {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                for ev in scroll_buf.drain(..) {
                    sender.send(Message::HostInput(ev))?;
                }
            }
            continue;
        }
        let mapped = state.map(raw, time_ms);
        match mapped {
            HostInputEvent::PointerMotion { .. } => {
                *pending_motion = Some(mapped);
            }
            non_motion => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                sender.send(Message::HostInput(non_motion))?;
            }
        }
    }
    Ok(())
}

/// Long-running libinput thread body. Owns `input_ctx`, drives an
/// `epoll` set on its fd, dispatches batches through [`process_batch`],
/// and flushes any leftover pending motion at the end of each batch so
/// the core never sees stale "latest motion" sitting in the channel.
///
/// `led_relay` carries lock-LED updates from the core thread (which
/// owns the XKB lock state) into this thread (which owns the libinput
/// devices); its eventfd sits in the same epoll set as the libinput fd.
///
/// Returns only on a fatal send error (channel closed = core gone).
pub(crate) fn run(
    input_ctx: SendContext,
    sender: CoreSender,
    fb_w: u32,
    fb_h: u32,
    control: std::sync::Arc<InputThreadControl>,
    led_relay: std::sync::Arc<crate::input::LedRelay>,
) -> io::Result<()> {
    let mut input_ctx = input_ctx;
    let mut state = LibinputThreadState::new(fb_w, fb_h);
    let mut pending_motion: Option<HostInputEvent> = None;
    let mut paused = false;

    const EPOLL_DATA_LIBINPUT: u64 = 0;
    const EPOLL_DATA_CONTROL: u64 = 1;
    const EPOLL_DATA_LEDS: u64 = 2;
    let input_epoll = Epoll::new(EpollCreateFlags::empty())
        .map_err(|err| io::Error::other(format!("input thread epoll_create: {err}")))?;
    let fd = input_ctx.fd();
    // SAFETY: `input_ctx` outlives this borrow because both live for
    // the duration of `run`.
    let borrow = unsafe { BorrowedFd::borrow_raw(fd) };
    input_epoll
        .add(
            borrow,
            EpollEvent::new(EpollFlags::EPOLLIN, EPOLL_DATA_LIBINPUT),
        )
        .map_err(|err| io::Error::other(format!("input thread epoll_add: {err}")))?;
    // SAFETY: `control` (Arc) outlives this borrow — held for the
    // duration of `run`.
    let control_borrow = unsafe { BorrowedFd::borrow_raw(control.fd()) };
    input_epoll
        .add(
            control_borrow,
            EpollEvent::new(EpollFlags::EPOLLIN, EPOLL_DATA_CONTROL),
        )
        .map_err(|err| io::Error::other(format!("input thread epoll_add (control): {err}")))?;
    // SAFETY: `led_relay` (Arc) outlives this borrow — held for the
    // duration of `run`.
    let led_borrow = unsafe { BorrowedFd::borrow_raw(led_relay.fd()) };
    input_epoll
        .add(
            led_borrow,
            EpollEvent::new(EpollFlags::EPOLLIN, EPOLL_DATA_LEDS),
        )
        .map_err(|err| io::Error::other(format!("input thread epoll_add (leds): {err}")))?;

    // Drain udev's initial device enumeration before waiting on
    // epoll. udev_assign_seat queues DeviceAdded events synchronously
    // but dispatch() must be called once to consume them. Without this
    // first dispatch, the very first epoll_wait blocks until any input
    // arrives, and the seat enumeration is silently held back. This is
    // also where `libinput: device added` first lands in the log.
    {
        let initial = match input_ctx.dispatch() {
            Ok(evs) => evs,
            Err(err) => {
                log::warn!("input thread: initial libinput dispatch: {err}");
                Vec::new()
            }
        };
        if !initial.is_empty() {
            let time_ms = current_time_ms();
            process_batch(&mut state, &sender, &mut pending_motion, initial, time_ms)?;
            if let Some(m) = pending_motion.take() {
                sender.send(Message::HostInput(m))?;
            }
        }
    }

    let mut buf = [EpollEvent::empty(); 4];
    loop {
        match input_epoll.wait(&mut buf, EpollTimeout::NONE) {
            Ok(n) => {
                if buf[..n].iter().any(|e| e.data() == EPOLL_DATA_CONTROL)
                    && let Some(command) = control.drain()
                {
                    paused = match command {
                        InputThreadCommand::Pause if !paused => {
                            input_ctx.suspend();
                            pending_motion = None;
                            // Forget held modifiers: their release events go
                            // to whoever owns the keyboard after the switch,
                            // not us, so the flags would otherwise stay stuck.
                            state.hotkey.reset();
                            true
                        }
                        InputThreadCommand::Resume if paused => match input_ctx.resume() {
                            Ok(()) => {
                                // Start clean on the VT we just acquired —
                                // any modifiers held during the switch were
                                // released while we weren't reading.
                                state.hotkey.reset();
                                // Drain any libinput resume-replay first so our
                                // synthetic releases land AFTER, not before.
                                let _ = input_ctx.dispatch();
                                // Forget held modifiers on VT-enter (Xorg-style):
                                // emit a KeyRelease for each modifier X-keycode so
                                // a modifier whose release was lost across the
                                // switch is cleared in xkb_state. Harmless when not
                                // held (release of an up key is a no-op); a still-
                                // held modifier re-presses on its next event.
                                // X keycodes (evdev+8): LCtrl37 RCtrl105 LAlt64
                                // RAlt108 LShift50 RShift62 LSuper133 RSuper134.
                                for kc in [37u8, 105, 64, 108, 50, 62, 133, 134] {
                                    let _ = sender.send(Message::HostInput(HostInputEvent::Key(
                                        HostKeyEvent {
                                            pressed: false,
                                            keycode: kc,
                                            time: current_time_ms(),
                                            root_x: state.cursor_x as i16,
                                            root_y: state.cursor_y as i16,
                                            event_x: state.cursor_x as i16,
                                            event_y: state.cursor_y as i16,
                                            state: 0,
                                        },
                                    )));
                                }
                                false
                            }
                            Err(err) => {
                                log::warn!("input thread: libinput resume failed: {err}");
                                true
                            }
                        },
                        _ => paused,
                    };
                }
                // Core-thread lock-LED update: drain the eventfd and
                // push the mask to every keyboard device. Falls through
                // to the libinput dispatch below (harmless when the
                // libinput fd wasn't the waker — dispatch just returns
                // no events).
                if buf[..n].iter().any(|e| e.data() == EPOLL_DATA_LEDS) {
                    let leds = input::Led::from_bits_truncate(led_relay.drain());
                    input_ctx.update_leds(leds);
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => {
                log::warn!("input thread: epoll_wait: {err}");
                continue;
            }
        }

        if !should_dispatch_batch(paused) {
            let _ = input_ctx.dispatch();
            continue;
        }

        let events = match input_ctx.dispatch() {
            Ok(evs) => evs,
            Err(err) => {
                log::warn!("input thread: libinput dispatch: {err}");
                continue;
            }
        };

        let time_ms = current_time_ms();
        process_batch(&mut state, &sender, &mut pending_motion, events, time_ms)?;
        // Flush any pending motion before the next epoll_wait so the
        // core sees end-of-burst movement promptly. A subsequent batch
        // whose first event is another motion just starts coalescing
        // fresh.
        if let Some(m) = pending_motion.take() {
            sender.send(Message::HostInput(m))?;
        }
    }
}

fn should_dispatch_batch(paused: bool) -> bool {
    !paused
}

fn current_time_ms() -> u32 {
    crate::clock::server_time_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::hotkey::{
        LINUX_KEY_BACKSPACE, LINUX_KEY_D, LINUX_KEY_ENTER, LINUX_KEY_LEFTALT, LINUX_KEY_LEFTCTRL,
        LINUX_KEY_RIGHTALT, LINUX_KEY_RIGHTCTRL,
    };
    use yserver_core::core_loop::channel;

    #[test]
    fn maps_relative_motion_to_clamped_absolute() {
        let mut s = LibinputThreadState::new(800, 600);
        // Center: (400, 300)
        assert_eq!(s.cursor(), (400.0, 300.0));
        let ev = s.map(
            InputEvent::PointerMotion {
                dx: 50.0,
                dy: -100.0,
            },
            0,
        );
        assert!(matches!(
            ev,
            HostInputEvent::PointerMotion { x: 450, y: 200, .. }
        ));
        // Walk past the right edge — clamps to fb_w-1.
        let _ = s.map(
            InputEvent::PointerMotion {
                dx: 1000.0,
                dy: 0.0,
            },
            0,
        );
        let (cx, _) = s.cursor();
        assert!((cx - 799.0).abs() < 0.5, "cursor_x = {cx}");
    }

    #[test]
    fn maps_absolute_motion_to_scanout_pixels() {
        let mut s = LibinputThreadState::new(800, 600);
        let ev = s.map(
            InputEvent::PointerMotionAbsolute {
                x_norm: 0.5,
                y_norm: 0.25,
            },
            42,
        );
        match ev {
            HostInputEvent::PointerMotion { x, y, time } => {
                assert_eq!(time, 42);
                // 0.5 * 799 ≈ 399.5 → 399 (truncation when cast to i32)
                assert!((x - 399).abs() <= 1, "x = {x}");
                // 0.25 * 599 ≈ 149.75 → 149
                assert!((y - 149).abs() <= 1, "y = {y}");
            }
            other => panic!("expected PointerMotion, got {other:?}"),
        }
    }

    #[test]
    fn maps_buttons_and_keys_without_state_mutation() {
        let mut s = LibinputThreadState::new(800, 600);
        let before = s.cursor();
        let btn = s.map(
            InputEvent::Button {
                code: 0x110,
                pressed: true,
            },
            7,
        );
        match btn {
            HostInputEvent::PointerButton {
                button,
                pressed,
                time,
            } => {
                assert_eq!(button, 0x110);
                assert!(pressed);
                assert_eq!(time, 7);
            }
            other => panic!("expected PointerButton, got {other:?}"),
        }
        let key = s.map(InputEvent::KeyPress { keycode: 30 }, 8);
        match key {
            HostInputEvent::Key(ev) => {
                assert!(ev.pressed);
                assert_eq!(ev.keycode, 38); // 30 + 8 (evdev → X11)
                assert_eq!(ev.time, 8);
            }
            other => panic!("expected Key, got {other:?}"),
        }
        assert_eq!(s.cursor(), before, "buttons/keys must not move the cursor");
    }

    /// Headline test from plan §E2: a batch of 5 motions + 1 button +
    /// 3 motions yields exactly three sender messages — last motion,
    /// button, last motion.
    #[test]
    fn process_batch_coalesces_consecutive_motions() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        let batch = vec![
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::Button {
                code: 1,
                pressed: true,
            },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
        ];
        process_batch(&mut state, &sender, &mut pending, batch, 100).unwrap();
        // End-of-batch flush (matches the production loop in `run`).
        if let Some(m) = pending.take() {
            sender.send(Message::HostInput(m)).unwrap();
        }

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(
            collected.len(),
            3,
            "expected 3 messages (motion, button, motion); got {}: {collected:?}",
            collected.len()
        );
        match &collected[0] {
            Message::HostInput(HostInputEvent::PointerMotion { x: 405, y: 305, .. }) => {}
            other => panic!("first message: {other:?}"),
        }
        match &collected[1] {
            Message::HostInput(HostInputEvent::PointerButton {
                button: 1,
                pressed: true,
                ..
            }) => {}
            other => panic!("second message: {other:?}"),
        }
        match &collected[2] {
            Message::HostInput(HostInputEvent::PointerMotion { x: 408, y: 308, .. }) => {}
            other => panic!("third message: {other:?}"),
        }
        // Silence unused warning on `poll` — we just need its waker
        // alive for the channel to function.
        drop(poll);
    }

    #[test]
    fn process_batch_carries_motion_across_batches() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        // Batch A: motion only — left in `pending`, nothing sent yet.
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::PointerMotion { dx: 5.0, dy: 0.0 }],
            1,
        )
        .unwrap();
        assert!(pending.is_some());
        let immediate: Vec<Message> = rx.try_recv_all().collect();
        assert!(immediate.is_empty(), "no flush yet, got {immediate:?}");

        // Batch B: motion then button — only the latest combined
        // motion + button get sent.
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::PointerMotion { dx: 10.0, dy: 0.0 },
                InputEvent::Button {
                    code: 1,
                    pressed: true,
                },
            ],
            2,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(collected.len(), 2);
        match &collected[0] {
            Message::HostInput(HostInputEvent::PointerMotion { x: 415, y: 300, .. }) => {}
            other => panic!("first message: {other:?}"),
        }
        match &collected[1] {
            Message::HostInput(HostInputEvent::PointerButton {
                button: 1,
                pressed: true,
                ..
            }) => {}
            other => panic!("second message: {other:?}"),
        }
        drop(poll);
    }

    #[test]
    fn ctrl_alt_backspace_emits_shutdown_and_drops_keypress() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_BACKSPACE,
                },
                // Anything after the zap is dropped — the server is
                // already shutting down. This press must NOT reach
                // the core.
                InputEvent::KeyPress {
                    keycode: 30, /* a */
                },
            ],
            0,
        )
        .unwrap();

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "expected Shutdown in {collected:?}",
        );
        assert!(
            !collected.iter().any(|m| matches!(
                m,
                Message::HostInput(HostInputEvent::Key(ev)) if ev.pressed && ev.keycode == 14 + 8
            )),
            "Backspace keypress must not be forwarded after zap, got {collected:?}",
        );
        // Modifier presses before the Backspace landed first; tolerate
        // those since they were valid client events at the time.
        drop(poll);
    }

    #[test]
    fn backspace_alone_does_not_zap() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::KeyPress {
                keycode: LINUX_KEY_BACKSPACE,
            }],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            !collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "Shutdown must not fire on lone Backspace, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn modifier_release_disarms_zap() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyRelease {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_BACKSPACE,
                },
            ],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            !collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "Shutdown must not fire after Ctrl release, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn right_modifiers_also_arm_zap() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_RIGHTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_RIGHTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_BACKSPACE,
                },
            ],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "right Ctrl + right Alt + Backspace must zap, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn ctrl_alt_enter_emits_dump_scanout_and_drops_keypress() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_ENTER,
                },
            ],
            0,
        )
        .unwrap();

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected.iter().any(|m| matches!(m, Message::DumpScanout)),
            "expected DumpScanout in {collected:?}",
        );
        assert!(
            !collected.iter().any(|m| matches!(
                m,
                Message::HostInput(HostInputEvent::Key(ev)) if ev.pressed && ev.keycode == 28 + 8
            )),
            "Enter keypress must not be forwarded after dump-scanout hotkey, got {collected:?}",
        );
        drop(poll);
    }

    /// Mirror of `ctrl_alt_enter_emits_dump_scanout_and_drops_keypress`
    /// for the per-drawable storage dump (Ctrl-Alt-D → SIGUSR2 path).
    #[test]
    fn ctrl_alt_d_emits_dump_drawables_and_drops_keypress() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_D,
                },
            ],
            0,
        )
        .unwrap();

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected
                .iter()
                .any(|m| matches!(m, Message::DumpDrawables)),
            "expected DumpDrawables in {collected:?}",
        );
        assert!(
            !collected.iter().any(|m| matches!(
                m,
                Message::HostInput(HostInputEvent::Key(ev)) if ev.pressed && u32::from(ev.keycode) == LINUX_KEY_D + 8
            )),
            "D keypress must not be forwarded after dump-drawables hotkey, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn decode_input_thread_command_and_batch_gate() {
        assert!(matches!(
            InputThreadCommand::from_raw(1),
            Some(InputThreadCommand::Pause)
        ));
        assert!(matches!(
            InputThreadCommand::from_raw(2),
            Some(InputThreadCommand::Resume)
        ));
        assert!(InputThreadCommand::from_raw(0).is_none());
        assert!(!should_dispatch_batch(true));
        assert!(should_dispatch_batch(false));
    }

    #[test]
    fn enter_alone_does_not_dump_scanout() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::KeyPress {
                keycode: LINUX_KEY_ENTER,
            }],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            !collected.iter().any(|m| matches!(m, Message::DumpScanout)),
            "DumpScanout must not fire on lone Enter, got {collected:?}",
        );
        drop(poll);
    }

    fn collect_button_codes(msgs: &[Message]) -> Vec<(u16, bool)> {
        msgs.iter()
            .filter_map(|m| match m {
                Message::HostInput(HostInputEvent::PointerButton {
                    button, pressed, ..
                }) => Some((*button, *pressed)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn scroll_one_click_down_emits_press_release_pair() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::PointerScroll {
                dx_v120: 0,
                dy_v120: 120,
            }],
            7,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(
            collect_button_codes(&collected),
            vec![(SYNTH_SCROLL_DOWN, true), (SYNTH_SCROLL_DOWN, false)],
            "expected one scroll-down press+release pair, got {collected:?}"
        );
        drop(poll);
    }

    #[test]
    fn scroll_accumulates_subclick_v120() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        // 60 + 30 + 40 = 130 → one click; remainder 10 banked.
        state.drain_scroll(0, 60, 0, &mut out);
        assert!(out.is_empty(), "60 < 120, no emission yet");
        state.drain_scroll(0, 30, 0, &mut out);
        assert!(out.is_empty(), "60 + 30 = 90 < 120");
        state.drain_scroll(0, 40, 0, &mut out);
        assert_eq!(
            out.len(),
            2,
            "60 + 30 + 40 = 130 should emit one press+release pair"
        );
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_DOWN,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn scroll_negative_v120_emits_scroll_up() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        state.drain_scroll(0, -120, 0, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_UP,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn scroll_multiple_clicks_in_one_event() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        // 480 v120 = exactly 4 clicks down.
        state.drain_scroll(0, 480, 0, &mut out);
        assert_eq!(out.len(), 8, "4 clicks × (press + release)");
        for chunk in out.chunks_exact(2) {
            assert!(matches!(
                chunk[0],
                HostInputEvent::PointerButton {
                    button: SYNTH_SCROLL_DOWN,
                    pressed: true,
                    ..
                }
            ));
            assert!(matches!(
                chunk[1],
                HostInputEvent::PointerButton {
                    button: SYNTH_SCROLL_DOWN,
                    pressed: false,
                    ..
                }
            ));
        }
    }

    #[test]
    fn scroll_horizontal_emits_buttons_6_7() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        state.drain_scroll(120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_RIGHT,
                pressed: true,
                ..
            }
        ));
        out.clear();
        state.drain_scroll(-120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_LEFT,
                pressed: true,
                ..
            }
        ));
    }
}
