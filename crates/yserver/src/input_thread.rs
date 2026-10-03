//! Sender-only libinput thread for the single-threaded core.
//!
//! The thread owns the `SendContext` and an `epoll` set wrapping the
//! libinput fd. Each batch of `crate::input::InputEvent`s gets mapped
//! to `HostInputEvent`s and pushed onto the core's message channel.
//! Consecutive `PointerMotion` events from the same origin and motion mode
//! are coalesced — at most one compatible motion stays in flight to the core
//! at any given moment. Deltas sum, while the latest absolute position wins.
//! Buttons and keys are never coalesced and flush any pending motion
//! immediately.
//!
//! Absolute-device mapping and the current virtual framebuffer extent live
//! on this thread. Physical relative motion carries its fractional delta to
//! KMS, which integrates it against the authoritative cursor position.
//!
//! Spec: `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`
//! Plan: `docs/superpowers/plans/2026-05-06-single-threaded-core.md` §E2.

use std::{
    collections::{HashMap, VecDeque},
    io,
    os::fd::{AsFd, AsRawFd},
    sync::Mutex,
    time::Instant,
};

#[cfg(target_os = "linux")]
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
#[cfg(target_os = "freebsd")]
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
use nix::sys::eventfd::{EfdFlags, EventFd};
use yserver_core::{
    core_loop::{
        CoreSender, HostInputEvent, InputOrigin, Message, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT,
        SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_UP,
    },
    host_x11::HostKeyEvent,
    xinput::{
        InputSourceId,
        libinput_props::{DeviceConfigChange, DeviceConfigError, DeviceConfigToken},
    },
};

use crate::input::{
    InputEvent, SendContext,
    hotkey::{Hotkey, HotkeyDetector},
};

/// Absolute pointer mapping position + framebuffer dimensions held on the
/// libinput thread.
#[derive(Debug, Clone)]
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
    scroll_accum_by_source: HashMap<InputSourceId, (i32, i32)>,
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
            scroll_accum_by_source: HashMap::new(),
        }
    }

    #[must_use]
    pub fn cursor(&self) -> (f64, f64) {
        (self.cursor_x, self.cursor_y)
    }

    /// Update the virtual framebuffer extent used for pointer clamping.
    ///
    /// Called whenever the logical screen size changes (hotplug or
    /// `RRSetScreenSize`). The last mapped position is not reclamped here;
    /// absolute input is mapped against the new extent on the next event.
    pub fn set_extent(&mut self, fb_w: u32, fb_h: u32) {
        self.fb_w = fb_w;
        self.fb_h = fb_h;
    }

    /// Translate one libinput event into a `HostInputEvent`.
    ///
    /// `time_ms` lets tests pin the timestamp; production callers pass
    /// the wall clock.
    pub(crate) fn map(&mut self, ev: InputEvent, time_ms: u32) -> HostInputEvent {
        match ev {
            InputEvent::KeyPress { source_id, keycode } => HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::Physical(source_id),
                pressed: true,
                keycode: ((keycode + 8) & 0xff) as u8,
                time: time_ms,
                root_x: self.cursor_x as i16,
                root_y: self.cursor_y as i16,
                event_x: self.cursor_x as i16,
                event_y: self.cursor_y as i16,
                state: 0,
            }),
            InputEvent::KeyRelease { source_id, keycode } => HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::Physical(source_id),
                pressed: false,
                keycode: ((keycode + 8) & 0xff) as u8,
                time: time_ms,
                root_x: self.cursor_x as i16,
                root_y: self.cursor_y as i16,
                event_x: self.cursor_x as i16,
                event_y: self.cursor_y as i16,
                state: 0,
            }),
            InputEvent::PointerMotion { source_id, dx, dy } => {
                HostInputEvent::PointerMotion {
                    origin: InputOrigin::Physical(source_id),
                    x: self.cursor_x as i32,
                    y: self.cursor_y as i32,
                    time: time_ms,
                    relative: true,
                    // Raw relative delta = the physical libinput motion
                    // (pre-clamp) so XI2 RawMotion reports true deltas, as
                    // Xorg's set_raw_valuators does. (Sub-pixel fraction is
                    // rounded per-event; acceptable for relative-mode apps.)
                    dx: dx.round() as i32,
                    dy: dy.round() as i32,
                    motion_delta: Some([dx, dy]),
                }
            }
            InputEvent::PointerMotionAbsolute {
                source_id,
                x_norm,
                y_norm,
            } => {
                let (old_cx, old_cy) = (self.cursor_x, self.cursor_y);
                self.cursor_x = x_norm.clamp(0.0, 1.0) * (f64::from(self.fb_w).max(1.0) - 1.0);
                self.cursor_y = y_norm.clamp(0.0, 1.0) * (f64::from(self.fb_h).max(1.0) - 1.0);
                HostInputEvent::PointerMotion {
                    origin: InputOrigin::Physical(source_id),
                    x: self.cursor_x as i32,
                    y: self.cursor_y as i32,
                    time: time_ms,
                    relative: false,
                    // Absolute devices have no native relative delta — report
                    // the change in mapped position (Xorg does the same for
                    // absolute-device raw events).
                    dx: (self.cursor_x - old_cx).round() as i32,
                    dy: (self.cursor_y - old_cy).round() as i32,
                    motion_delta: None,
                }
            }
            InputEvent::Button {
                source_id,
                code,
                pressed,
            } => HostInputEvent::PointerButton {
                origin: InputOrigin::Physical(source_id),
                button: u16::try_from(code).unwrap_or(u16::MAX),
                pressed,
                time: time_ms,
            },
            // PointerScroll is fanned out separately via `drain_scroll`
            // because it can map to N (≥ 0) press+release pairs depending
            // on accumulated v120. Reaching here means a caller forgot
            // to route it; map to a no-op-ish placeholder.
            InputEvent::PointerScroll { source_id, .. } => HostInputEvent::PointerButton {
                origin: InputOrigin::Physical(source_id),
                button: u16::MAX,
                pressed: false,
                time: time_ms,
            },
            // Handled in process_batch (flushes motion + resets the scroll
            // accumulator) before map() is reached.
            InputEvent::PointerScrollStop { .. } => {
                unreachable!("PointerScrollStop is routed in process_batch before map()")
            }
            // DeviceAdded/Removed are forwarded by process_batch before
            // reaching map(); reaching here is a routing bug, so fail loud.
            InputEvent::DeviceAdded(_)
            | InputEvent::DeviceSuspended { .. }
            | InputEvent::DeviceResumed(_)
            | InputEvent::DeviceRemoved { .. } => {
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
        source_id: InputSourceId,
        dx_v120: i32,
        dy_v120: i32,
        time_ms: u32,
        out: &mut Vec<HostInputEvent>,
    ) {
        let (accum_x, accum_y) = self.scroll_accum_by_source.entry(source_id).or_default();
        *accum_x = accum_x.saturating_add(dx_v120);
        *accum_y = accum_y.saturating_add(dy_v120);

        // Vertical first (more common; matches X11 button-4/5 priority).
        while *accum_y >= Self::V120_PER_CLICK {
            *accum_y -= Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_DOWN, time_ms, source_id);
        }
        while *accum_y <= -Self::V120_PER_CLICK {
            *accum_y += Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_UP, time_ms, source_id);
        }
        while *accum_x >= Self::V120_PER_CLICK {
            *accum_x -= Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_RIGHT, time_ms, source_id);
        }
        while *accum_x <= -Self::V120_PER_CLICK {
            *accum_x += Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_LEFT, time_ms, source_id);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputThreadCommand {
    Pause,
    Resume,
}

/// Direct-mode input-thread control channel.
///
/// Carries three kinds of message to the input thread, multiplexed on a
/// single `eventfd` wakeup: the FIFO pause/resume `commands` (for
/// VT-switch suspend/resume), a queue of `configs` — source-targeted
/// client `xinput set-prop` device-config writes that must be applied on the
/// thread that owns the libinput handles, and a
/// latched `pending_resize` — the latest virtual framebuffer extent to
/// apply to the absolute-device mapping extent (only the newest value matters, so
/// this uses a pair of atomics rather than a queue).
#[derive(Debug)]
pub(crate) struct InputThreadControl {
    commands: Mutex<VecDeque<InputThreadCommand>>,
    configs: Mutex<VecDeque<(DeviceConfigToken, InputSourceId, DeviceConfigChange)>>,
    efd: EventFd,
    /// Latched pending resize. Written by the core thread via
    /// `push_resize`; read+cleared by the input thread via `take_resize`.
    /// Latest pending virtual-framebuffer extent, or `None` when no
    /// resize is pending. A `Mutex` (not a pair of atomics) so the `(w, h)`
    /// pair is read/written atomically — two independent atomics let
    /// `take_resize` observe a torn `(new_w, old_h)` and clamp the cursor
    /// against a wrong extent. The resize path is rare (resize/hotplug),
    /// so the lock cost is irrelevant.
    pending_resize: Mutex<Option<(u32, u32)>>,
}

impl InputThreadControl {
    pub(crate) fn new() -> io::Result<Self> {
        let efd = EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
            .map_err(|e| io::Error::other(format!("InputThreadControl eventfd: {e}")))?;
        Ok(Self {
            commands: Mutex::new(VecDeque::new()),
            configs: Mutex::new(VecDeque::new()),
            efd,
            pending_resize: Mutex::new(None),
        })
    }

    pub(crate) fn pause(&self) {
        self.enqueue(InputThreadCommand::Pause);
    }

    pub(crate) fn resume(&self) {
        self.enqueue(InputThreadCommand::Resume);
    }

    fn enqueue(&self, command: InputThreadCommand) {
        let mut queue = match self.commands.lock() {
            Ok(queue) => queue,
            Err(poisoned) => {
                log::error!("InputThreadControl: command queue mutex poisoned; recovering it");
                poisoned.into_inner()
            }
        };
        queue.push_back(command);
        drop(queue);
        self.wake();
    }

    /// Enqueue a device-config write for the input thread to apply to its
    /// own libinput device map. Async by nature: the apply (and any
    /// libinput rejection) happens on the next thread wakeup, so callers
    /// cannot observe the result here.
    pub(crate) fn push_config(
        &self,
        token: DeviceConfigToken,
        source: InputSourceId,
        change: DeviceConfigChange,
    ) {
        if let Ok(mut q) = self.configs.lock() {
            q.push_back((token, source, change));
        }
        self.wake();
    }

    pub(crate) fn fd(&self) -> std::os::fd::RawFd {
        self.efd.as_fd().as_raw_fd()
    }

    pub(crate) fn drain(&self) -> Vec<InputThreadCommand> {
        let _ = self.efd.read();
        let mut queue = match self.commands.lock() {
            Ok(queue) => queue,
            Err(poisoned) => {
                log::error!("InputThreadControl: command queue mutex poisoned; recovering it");
                poisoned.into_inner()
            }
        };
        queue.drain(..).collect()
    }

    /// Drain all queued device-config writes. The eventfd is consumed by
    /// [`drain`]; this only empties the config queue, so the caller must
    /// invoke it on every control wakeup (a config-only push latches no
    /// `command`, so `drain` alone would skip it).
    pub(crate) fn take_configs(
        &self,
    ) -> Vec<(DeviceConfigToken, InputSourceId, DeviceConfigChange)> {
        self.configs
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Push a new virtual framebuffer extent to the input thread so its
    /// absolute-device mapping uses the correct range after a resize or
    /// hotplug.  Only the latest value matters; subsequent calls before
    /// the thread drains the event overwrite the previous value.
    ///
    /// A zero `fb_w` is a no-op (zero is the sentinel for "no pending
    /// resize").
    pub(crate) fn push_resize(&self, fb_w: u32, fb_h: u32) {
        if fb_w == 0 {
            return;
        }
        if let Ok(mut slot) = self.pending_resize.lock() {
            *slot = Some((fb_w, fb_h));
        }
        self.wake();
    }

    /// Read and clear any pending resize pushed by [`push_resize`].
    /// Returns `Some((fb_w, fb_h))` if a resize is pending, `None`
    /// otherwise.  Called on the input thread inside the control-wakeup
    /// handler.
    pub(crate) fn take_resize(&self) -> Option<(u32, u32)> {
        self.pending_resize
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    fn wake(&self) {
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

fn push_button_click(
    out: &mut Vec<HostInputEvent>,
    button: u16,
    time_ms: u32,
    source_id: InputSourceId,
) {
    out.push(HostInputEvent::PointerButton {
        origin: InputOrigin::Physical(source_id),
        button,
        pressed: true,
        time: time_ms,
    });
    out.push(HostInputEvent::PointerButton {
        origin: InputOrigin::Physical(source_id),
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
                // core to dump the scanout.
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                log::info!("yserver: Ctrl-Alt-Enter pressed — dumping scanout");
                sender.send(Message::DumpScanout)?;
                continue;
            }
            Some(Hotkey::DumpDrawables) => {
                // Mirror DumpScanout: flush queued motion, drop the
                // F12 keypress itself, ask the core to dump
                // per-drawable storage.
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                log::info!("yserver: Ctrl-Alt-F12 pressed — dumping drawables");
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
        match raw {
            InputEvent::DeviceAdded(info) => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                sender.send(Message::HostInput(HostInputEvent::DeviceAdded(info)))?;
                continue;
            }
            InputEvent::DeviceResumed(info) => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                sender.send(Message::HostInput(HostInputEvent::DeviceResumed(info)))?;
                continue;
            }
            InputEvent::DeviceSuspended { source_id } => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                state.scroll_accum_by_source.remove(&source_id);
                sender.send(Message::HostInput(HostInputEvent::DeviceSuspended {
                    source_id,
                }))?;
                continue;
            }
            InputEvent::DeviceRemoved { source_id } => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                state.scroll_accum_by_source.remove(&source_id);
                sender.send(Message::HostInput(HostInputEvent::DeviceRemoved {
                    source_id,
                }))?;
                continue;
            }
            _ => {}
        }
        // Scroll fans out separately because one InputEvent may map to
        // zero or many press+release pairs depending on accumulated v120.
        if let InputEvent::PointerScroll {
            source_id,
            dx_v120,
            dy_v120,
        } = raw
        {
            scroll_buf.clear();
            state.drain_scroll(source_id, dx_v120, dy_v120, time_ms, &mut scroll_buf);
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
        // Fingers lifted from a two-finger scroll: discard the sub-click
        // remainder (matches Xorg — a partial click doesn't fire on lift) and
        // forward the stop so the backend emits a delta-0 XI2 scroll motion.
        // GDK reads that as `scroll.is_stop`, which commits a Firefox
        // history-swipe (bug 1539730).
        if let InputEvent::PointerScrollStop { source_id } = raw {
            state.scroll_accum_by_source.remove(&source_id);
            if let Some(m) = pending_motion.take() {
                sender.send(Message::HostInput(m))?;
            }
            sender.send(Message::HostInput(HostInputEvent::PointerScrollStop {
                origin: InputOrigin::Physical(source_id),
                time: time_ms,
            }))?;
            continue;
        }
        let mapped = state.map(raw, time_ms);
        match mapped {
            HostInputEvent::PointerMotion {
                x,
                y,
                time,
                relative,
                dx,
                dy,
                origin,
                motion_delta,
            } => {
                // Coalesce only within one producer and motion mode. Keep
                // the latest absolute position while preserving both raw
                // integer deltas and the fractional physical delta.
                let compatible = pending_motion.as_ref().is_some_and(|pending| {
                    matches!(pending,
                        HostInputEvent::PointerMotion {
                            origin: pending_origin,
                            relative: pending_relative,
                            motion_delta: pending_delta,
                            ..
                        } if *pending_origin == origin
                            && *pending_relative == relative
                            && pending_delta.is_some() == motion_delta.is_some())
                });
                let (sum_dx, sum_dy, sum_motion_delta) = if compatible {
                    let Some(HostInputEvent::PointerMotion {
                        dx: previous_dx,
                        dy: previous_dy,
                        motion_delta: previous_delta,
                        ..
                    }) = pending_motion.as_ref()
                    else {
                        unreachable!("compatible pending motion must be a motion")
                    };
                    (
                        dx + previous_dx,
                        dy + previous_dy,
                        match (previous_delta, motion_delta) {
                            (Some(previous), Some(current)) => {
                                Some([previous[0] + current[0], previous[1] + current[1]])
                            }
                            (None, None) => None,
                            _ => unreachable!("compatible motion deltas have matching modes"),
                        },
                    )
                } else {
                    if let Some(m) = pending_motion.take() {
                        sender.send(Message::HostInput(m))?;
                    }
                    (dx, dy, motion_delta)
                };
                *pending_motion = Some(HostInputEvent::PointerMotion {
                    origin,
                    x,
                    y,
                    time,
                    relative,
                    dx: sum_dx,
                    dy: sum_dy,
                    motion_delta: sum_motion_delta,
                });
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

fn reset_hotkeys_after_vt_pause(state: &mut LibinputThreadState) {
    state.hotkey.reset();
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
    initial_events: Vec<InputEvent>,
    sender: CoreSender,
    fb_w: u32,
    fb_h: u32,
    init_cursor_x: i32,
    init_cursor_y: i32,
    control: std::sync::Arc<InputThreadControl>,
    led_relay: std::sync::Arc<crate::input::LedRelay>,
) -> io::Result<()> {
    let mut input_ctx = input_ctx;
    let mut state = LibinputThreadState::new(fb_w, fb_h);
    // Seed the cursor at the primary-output centre (Xorg-style startup warp) so
    // it agrees with the core's seeded position; otherwise the first relative
    // motion would snap the pointer back toward the framebuffer centre (the
    // monitor seam on a multi-head layout).
    state.cursor_x = f64::from(init_cursor_x);
    state.cursor_y = f64::from(init_cursor_y);
    let mut pending_motion: Option<HostInputEvent> = None;
    let mut paused = false;

    const TOKEN_LIBINPUT: u64 = 0;
    const TOKEN_CONTROL: u64 = 1;
    const TOKEN_LEDS: u64 = 2;

    // --- epoll setup (Linux) ---
    #[cfg(target_os = "linux")]
    let input_poller = {
        use std::os::fd::BorrowedFd;
        let epoll = Epoll::new(EpollCreateFlags::empty())
            .map_err(|err| io::Error::other(format!("input thread epoll_create: {err}")))?;
        let fd = input_ctx.fd();
        let borrow = unsafe { BorrowedFd::borrow_raw(fd) };
        epoll
            .add(borrow, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_LIBINPUT))
            .map_err(|err| io::Error::other(format!("input thread epoll_add: {err}")))?;
        let control_borrow = unsafe { BorrowedFd::borrow_raw(control.fd()) };
        epoll
            .add(
                control_borrow,
                EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_CONTROL),
            )
            .map_err(|err| io::Error::other(format!("input thread epoll_add (control): {err}")))?;
        let led_borrow = unsafe { BorrowedFd::borrow_raw(led_relay.fd()) };
        epoll
            .add(led_borrow, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_LEDS))
            .map_err(|err| io::Error::other(format!("input thread epoll_add (leds): {err}")))?;
        epoll
    };

    // --- kqueue setup (FreeBSD) ---
    #[cfg(target_os = "freebsd")]
    let input_poller = {
        let kq =
            Kqueue::new().map_err(|err| io::Error::other(format!("input thread kqueue: {err}")))?;
        let fd = input_ctx.fd();
        let changes = [
            KEvent::new(
                fd as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD,
                FilterFlag::empty(),
                0,
                TOKEN_LIBINPUT as isize,
            ),
            KEvent::new(
                control.fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD,
                FilterFlag::empty(),
                0,
                TOKEN_CONTROL as isize,
            ),
            KEvent::new(
                led_relay.fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD,
                FilterFlag::empty(),
                0,
                TOKEN_LEDS as isize,
            ),
        ];
        let mut out = Vec::new();
        kq.kevent(
            &changes,
            &mut out,
            Some(libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .map_err(|err| io::Error::other(format!("input thread kevent register: {err}")))?;
        kq
    };

    // The initial seat enumeration was drained by the caller on the main
    // thread (so it could verify at least one input device opened before
    // signalling readiness — issue #64); process those events here. This is
    // also where `libinput: device added` first lands in the log.
    if !initial_events.is_empty() {
        let time_ms = current_time_ms();
        process_batch(
            &mut state,
            &sender,
            &mut pending_motion,
            initial_events,
            time_ms,
        )?;
        if let Some(m) = pending_motion.take() {
            sender.send(Message::HostInput(m))?;
        }
    }

    // Mouse-hotplug retry window (project_mouse_hotplug_lost_wakeup). After a
    // device add/remove, a re-enumerated sibling device's open can be DEFERRED
    // by a lagging udev uaccess ACL; the level-triggered libinput fd does NOT
    // re-wake once that udev event is consumed, so without a timeout the thread
    // would block until unrelated input arrives. While armed, give the poller
    // a ~250ms timeout so we re-dispatch and complete the deferred open.
    let mut hotplug_retry_until: Option<std::time::Instant> = None;
    let mut resume_retry_window: Option<(crate::input::context::ResumeWindowToken, Instant)> = None;
    let mut waiting_device_configs = VecDeque::new();

    // Platform-specific event buffer.
    #[cfg(target_os = "linux")]
    let mut buf = [EpollEvent::empty(); 4];
    #[cfg(target_os = "freebsd")]
    let mut kq_buf: Vec<KEvent> = vec![
        KEvent::new(
            0,
            EventFilter::EVFILT_READ,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0isize
        );
        4
    ];

    loop {
        finish_expired_resume_window(
            &mut input_ctx,
            &mut resume_retry_window,
            &mut state,
            &sender,
            &mut pending_motion,
            &mut waiting_device_configs,
        )?;
        if hotplug_retry_until.is_some_and(|until| Instant::now() >= until) {
            hotplug_retry_until = None;
        }
        // --- poll wait ---
        let (got_control, got_leds);

        let poll_until = match (hotplug_retry_until, resume_retry_window) {
            (Some(hotplug), Some((_, resume))) => Some(hotplug.max(resume)),
            (Some(hotplug), None) => Some(hotplug),
            (None, Some((_, resume))) => Some(resume),
            (None, None) => None,
        };

        #[cfg(target_os = "linux")]
        {
            let timeout = match poll_until {
                Some(until) => {
                    let now = std::time::Instant::now();
                    let ms =
                        u16::try_from(until.saturating_duration_since(now).as_millis().min(250))
                            .unwrap_or(250);
                    EpollTimeout::from(ms.max(1))
                }
                None => EpollTimeout::NONE,
            };
            match input_poller.wait(&mut buf, timeout) {
                Ok(n) => {
                    got_control = buf[..n].iter().any(|e| e.data() == TOKEN_CONTROL);
                    got_leds = buf[..n].iter().any(|e| e.data() == TOKEN_LEDS);
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(err) => {
                    log::warn!("input thread: epoll_wait: {err}");
                    continue;
                }
            }
        }

        #[cfg(target_os = "freebsd")]
        {
            let timeout = match poll_until {
                Some(until) => {
                    let now = std::time::Instant::now();
                    let dur = until.saturating_duration_since(now);
                    let ms = dur.as_millis().min(250) as i64;
                    Some(libc::timespec {
                        tv_sec: ms / 1000,
                        tv_nsec: (ms % 1000) * 1_000_000,
                    })
                }
                None => None,
            };
            match input_poller.kevent(&[], &mut kq_buf, timeout) {
                Ok(n) => {
                    got_control = kq_buf[..n]
                        .iter()
                        .any(|e| e.udata() == TOKEN_CONTROL as isize);
                    got_leds = kq_buf[..n].iter().any(|e| e.udata() == TOKEN_LEDS as isize);
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(err) => {
                    log::warn!("input thread: kevent: {err}");
                    continue;
                }
            }
        }

        // --- common dispatch (platform-independent) ---
        if got_control {
            let commands = control.drain();
            process_config_commands(
                control.take_configs(),
                &sender,
                &mut waiting_device_configs,
                |source, change| {
                    let result = input_ctx.apply_device_config(source, change);
                    let may_wait = matches!(result, Err(DeviceConfigError::SourceGone))
                        && input_ctx.can_wait_for_device_config(source);
                    (result, may_wait)
                },
            )?;
            if let Some((fw, fh)) = control.take_resize() {
                log::debug!("input thread: updating cursor extent to {fw}×{fh}");
                state.set_extent(fw, fh);
            }
            for command in commands {
                paused = match command {
                    InputThreadCommand::Pause if !paused => {
                        if let Some(m) = pending_motion.take() {
                            sender.send(Message::HostInput(m))?;
                        }
                        let events = input_ctx.suspend();
                        process_batch(
                            &mut state,
                            &sender,
                            &mut pending_motion,
                            events,
                            current_time_ms(),
                        )?;
                        if let Some(m) = pending_motion.take() {
                            sender.send(Message::HostInput(m))?;
                        }
                        resume_retry_window = None;
                        hotplug_retry_until = None;
                        pending_motion = None;
                        reset_hotkeys_after_vt_pause(&mut state);
                        true
                    }
                    InputThreadCommand::Resume if paused => match input_ctx.resume() {
                        Ok(events) => {
                            resume_retry_window = input_ctx.resume_window();
                            hotplug_retry_until = resume_retry_window.map(|(_, deadline)| deadline);
                            // Context::resume performs the initial libinput
                            // dispatch internally. If that dispatch returned
                            // after the fixed continuation deadline, retire
                            // unmatched sources before forwarding its batch.
                            finish_expired_resume_window(
                                &mut input_ctx,
                                &mut resume_retry_window,
                                &mut state,
                                &sender,
                                &mut pending_motion,
                                &mut waiting_device_configs,
                            )?;
                            state.hotkey.reset();
                            let lifecycle_events = snapshot_lifecycle_events_for_waiters(
                                &waiting_device_configs,
                                &events,
                            );
                            process_batch(
                                &mut state,
                                &sender,
                                &mut pending_motion,
                                events,
                                current_time_ms(),
                            )?;
                            if let Some(lifecycle_events) = lifecycle_events {
                                service_waiting_device_configs(
                                    &sender,
                                    &mut waiting_device_configs,
                                    &lifecycle_events,
                                    |source, change| input_ctx.apply_device_config(source, change),
                                )?;
                            }
                            if let Some(m) = pending_motion.take() {
                                sender.send(Message::HostInput(m))?;
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
        }
        if got_leds {
            let leds = input::Led::from_bits_truncate(led_relay.drain());
            input_ctx.update_leds(leds);
        }

        finish_expired_resume_window(
            &mut input_ctx,
            &mut resume_retry_window,
            &mut state,
            &sender,
            &mut pending_motion,
            &mut waiting_device_configs,
        )?;

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

        finish_expired_resume_window(
            &mut input_ctx,
            &mut resume_retry_window,
            &mut state,
            &sender,
            &mut pending_motion,
            &mut waiting_device_configs,
        )?;

        let device_change = events.iter().any(|e| {
            matches!(
                e,
                InputEvent::DeviceAdded(_) | InputEvent::DeviceRemoved { .. }
            )
        });
        let lifecycle_events =
            snapshot_lifecycle_events_for_waiters(&waiting_device_configs, &events);
        let time_ms = current_time_ms();
        process_batch(&mut state, &sender, &mut pending_motion, events, time_ms)?;
        if let Some(lifecycle_events) = lifecycle_events {
            service_waiting_device_configs(
                &sender,
                &mut waiting_device_configs,
                &lifecycle_events,
                |source, change| input_ctx.apply_device_config(source, change),
            )?;
        }
        if device_change {
            hotplug_retry_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(2500));
        }
        if let Some(m) = pending_motion.take() {
            sender.send(Message::HostInput(m))?;
        }
    }
}

fn should_dispatch_batch(paused: bool) -> bool {
    !paused
}

fn finish_expired_resume_window(
    input_ctx: &mut SendContext,
    window: &mut Option<(crate::input::context::ResumeWindowToken, Instant)>,
    state: &mut LibinputThreadState,
    sender: &CoreSender,
    pending_motion: &mut Option<HostInputEvent>,
    waiting_device_configs: &mut VecDeque<(DeviceConfigToken, InputSourceId, DeviceConfigChange)>,
) -> io::Result<()> {
    let Some((token, deadline)) = *window else {
        return Ok(());
    };
    let now = Instant::now();
    if now < deadline {
        return Ok(());
    }
    let events = input_ctx.finish_resume_window(token, now);
    *window = None;
    let lifecycle_events = snapshot_lifecycle_events_for_waiters(waiting_device_configs, &events);
    process_batch(state, sender, pending_motion, events, current_time_ms())?;
    if let Some(lifecycle_events) = lifecycle_events {
        service_waiting_device_configs(
            sender,
            waiting_device_configs,
            &lifecycle_events,
            |source, change| input_ctx.apply_device_config(source, change),
        )?;
    }
    if let Some(motion) = pending_motion.take() {
        sender.send(Message::HostInput(motion))?;
    }
    Ok(())
}

fn process_config_commands(
    configs: Vec<(DeviceConfigToken, InputSourceId, DeviceConfigChange)>,
    sender: &CoreSender,
    waiting: &mut VecDeque<(DeviceConfigToken, InputSourceId, DeviceConfigChange)>,
    mut apply: impl FnMut(InputSourceId, DeviceConfigChange) -> (Result<(), DeviceConfigError>, bool),
) -> io::Result<()> {
    for (token, source, change) in configs {
        let (result, may_wait) = apply(source, change);
        match result {
            Ok(()) => sender.send(Message::DeviceConfigResult {
                token,
                source,
                result: Ok(()),
            })?,
            Err(DeviceConfigError::SourceGone) if may_wait => {
                waiting.push_back((token, source, change));
            }
            Err(error) => sender.send(Message::DeviceConfigResult {
                token,
                source,
                result: Err(error),
            })?,
        }
    }
    Ok(())
}

fn service_waiting_device_configs(
    sender: &CoreSender,
    waiting: &mut VecDeque<(DeviceConfigToken, InputSourceId, DeviceConfigChange)>,
    lifecycle_events: &[InputEvent],
    mut apply: impl FnMut(InputSourceId, DeviceConfigChange) -> Result<(), DeviceConfigError>,
) -> io::Result<()> {
    let mut resumed = std::collections::HashSet::new();
    let mut removed = std::collections::HashSet::new();
    for event in lifecycle_events {
        match event {
            InputEvent::DeviceResumed(info) => {
                resumed.insert(info.source_id);
            }
            InputEvent::DeviceRemoved { source_id } => {
                removed.insert(*source_id);
            }
            _ => {}
        }
    }
    if resumed.is_empty() && removed.is_empty() {
        return Ok(());
    }

    let mut remaining = VecDeque::new();
    while let Some((token, source, change)) = waiting.pop_front() {
        if removed.contains(&source) {
            sender.send(Message::DeviceConfigResult {
                token,
                source,
                result: Err(DeviceConfigError::SourceGone),
            })?;
        } else if resumed.contains(&source) {
            let result = apply(source, change);
            sender.send(Message::DeviceConfigResult {
                token,
                source,
                result,
            })?;
        } else {
            remaining.push_back((token, source, change));
        }
    }
    *waiting = remaining;
    Ok(())
}

fn snapshot_lifecycle_events_for_waiters(
    waiting: &VecDeque<(DeviceConfigToken, InputSourceId, DeviceConfigChange)>,
    events: &[InputEvent],
) -> Option<Vec<InputEvent>> {
    (!waiting.is_empty()).then(|| events.to_vec())
}

fn current_time_ms() -> u32 {
    crate::clock::server_time_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resumed_info(source_id: InputSourceId) -> yserver_core::core_loop::DeviceInfo {
        yserver_core::core_loop::DeviceInfo {
            source_id,
            enabled: true,
            resume_key: None,
            capabilities: yserver_core::xinput::InputCapabilities {
                pointer: true,
                ..Default::default()
            },
            name: "test pointer".into(),
            device_node: "/dev/input/event99".into(),
            sysname: "event99".into(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: Default::default(),
        }
    }
    use crate::input::hotkey::{
        LINUX_KEY_BACKSPACE, LINUX_KEY_ENTER, LINUX_KEY_F12, LINUX_KEY_LEFTALT, LINUX_KEY_LEFTCTRL,
        LINUX_KEY_RIGHTALT, LINUX_KEY_RIGHTCTRL,
    };
    use yserver_core::{core_loop::channel, xinput::InputSourceId};

    const TEST_SOURCE_ID: InputSourceId = InputSourceId(1);

    #[test]
    fn maps_relative_motion_without_cursor_accumulation() {
        let mut s = LibinputThreadState::new(800, 600);
        // Center: (400, 300)
        assert_eq!(s.cursor(), (400.0, 300.0));
        let ev = s.map(
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 50.0,
                dy: -100.0,
            },
            0,
        );
        assert!(matches!(
            ev,
            HostInputEvent::PointerMotion {
                x: 400,
                y: 300,
                relative: true,
                dx: 50,
                dy: -100,
                motion_delta: Some([50.0, -100.0]),
                ..
            }
        ));
        // A large relative move is passed through without making the input
        // thread a second cursor authority or clamping the physical delta.
        let large = s.map(
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1000.0,
                dy: 0.0,
            },
            0,
        );
        assert!(matches!(
            large,
            HostInputEvent::PointerMotion {
                x: 400,
                y: 300,
                dx: 1000,
                motion_delta: Some([1000.0, 0.0]),
                ..
            }
        ));
        assert_eq!(s.cursor(), (400.0, 300.0));
    }

    /// After `set_extent` is called with the new virtual screen size, absolute
    /// device mapping must cover the *new* virtual extent.
    /// Regression guard for the 2-monitor hotplug bug: boot on a single
    /// 2560-wide screen, plug in a second screen → virtual width becomes
    /// 5120; cursor was stuck at x=2559 until the server restarted.
    #[test]
    fn set_extent_updates_absolute_mapping_past_old_right_edge() {
        // Boot on a single 2560×1440 display.
        let mut s = LibinputThreadState::new(2560, 1440);
        // An absolute event reaches the right edge of the single-monitor extent.
        let _ = s.map(
            InputEvent::PointerMotionAbsolute {
                source_id: TEST_SOURCE_ID,
                x_norm: 1.0,
                y_norm: 0.5,
            },
            0,
        );
        let (cx_before, _) = s.cursor();
        assert!(
            (cx_before - 2559.0).abs() < 0.5,
            "cursor should be clamped to 2559 before resize, got {cx_before}"
        );

        // Second monitor plugged in → virtual screen grows to 5120×1440.
        s.set_extent(5120, 1440);

        // Move right: must now cross 2560 and reach the new far edge.
        let ev = s.map(
            InputEvent::PointerMotionAbsolute {
                source_id: TEST_SOURCE_ID,
                x_norm: 0.8,
                y_norm: 0.5,
            },
            0,
        );
        match ev {
            HostInputEvent::PointerMotion { x, .. } => {
                assert!(
                    x > 2559,
                    "cursor must cross old right edge after set_extent; got x={x}"
                );
                assert!(
                    x <= 5119,
                    "cursor must not exceed new right edge (5119); got x={x}"
                );
            }
            other => panic!("expected PointerMotion, got {other:?}"),
        }

        // Absolute devices map to the full new extent.
        let _ = s.map(
            InputEvent::PointerMotionAbsolute {
                source_id: TEST_SOURCE_ID,
                x_norm: 1.0,
                y_norm: 0.5,
            },
            0,
        );
        let (cx_after, _) = s.cursor();
        assert!(
            (cx_after - 5119.0).abs() < 0.5,
            "cursor should clamp to new right edge 5119, got {cx_after}"
        );
    }

    /// `take_resize` / `push_resize` round-trip: the input thread's control
    /// channel must deliver the latest extent and return `None` on a
    /// subsequent drain.
    #[test]
    fn push_resize_take_resize_round_trip() {
        let ctrl = InputThreadControl::new().expect("control");
        assert_eq!(ctrl.take_resize(), None, "no pending resize initially");
        ctrl.push_resize(5120, 1440);
        assert_eq!(
            ctrl.take_resize(),
            Some((5120, 1440)),
            "take_resize must return the pushed extent"
        );
        // Second take returns None: resize was consumed.
        assert_eq!(
            ctrl.take_resize(),
            None,
            "extent must be consumed after take"
        );
    }

    #[test]
    fn resize_control_preserves_queued_relative_motion() {
        let ctrl = InputThreadControl::new().expect("control");
        let mut state = LibinputThreadState::new(800, 600);
        let (poll, sender, rx) = channel().expect("channel");
        let mut pending = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 0.4,
                dy: 0.0,
            }],
            0,
        )
        .unwrap();
        ctrl.push_resize(5120, 1440);
        assert_eq!(
            ctrl.take_resize(),
            Some((5120, 1440)),
            "resize control remains available"
        );
        let Some(HostInputEvent::PointerMotion {
            x,
            y,
            motion_delta: Some([dx, dy]),
            ..
        }) = pending
        else {
            panic!("relative motion remains pending across resize control");
        };
        assert_eq!((x, y), (400, 300));
        assert!((dx - 0.4).abs() < f64::EPSILON);
        assert_eq!(dy, 0.0);
        assert!(rx.try_recv_all().next().is_none());
        drop(poll);
    }

    /// An absolute/touch event updates the mapped position, while later
    /// relative input leaves that position alone for KMS to integrate.
    #[test]
    fn absolute_then_relative_mapping_keeps_relative_position_kms_owned() {
        let mut s = LibinputThreadState::new(800, 600);
        let absolute = s.map(
            InputEvent::PointerMotionAbsolute {
                source_id: TEST_SOURCE_ID,
                x_norm: 99.0 / 799.0,
                y_norm: 50.0 / 599.0,
            },
            0,
        );
        assert!(matches!(
            absolute,
            HostInputEvent::PointerMotion { x: 99, y: 50, .. }
        ));
        let absolute_position = s.cursor();
        let ev = s.map(
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: -100.0,
                dy: 0.0,
            },
            0,
        );
        match ev {
            HostInputEvent::PointerMotion { x, y, .. } => assert_eq!((x, y), (99, 50)),
            other => panic!("expected PointerMotion, got {other:?}"),
        }
        assert_eq!(s.cursor(), absolute_position);
    }

    /// Only the latest push_resize survives — older values are overwritten.
    #[test]
    fn push_resize_overwrites_stale_extent() {
        let ctrl = InputThreadControl::new().expect("control");
        ctrl.push_resize(3840, 2160);
        ctrl.push_resize(5120, 1440);
        assert_eq!(
            ctrl.take_resize(),
            Some((5120, 1440)),
            "latest resize must win"
        );
    }

    #[test]
    fn maps_absolute_motion_to_scanout_pixels() {
        let mut s = LibinputThreadState::new(800, 600);
        let ev = s.map(
            InputEvent::PointerMotionAbsolute {
                source_id: TEST_SOURCE_ID,
                x_norm: 0.5,
                y_norm: 0.25,
            },
            42,
        );
        match ev {
            HostInputEvent::PointerMotion { x, y, time, .. } => {
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
                source_id: TEST_SOURCE_ID,
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
                ..
            } => {
                assert_eq!(button, 0x110);
                assert!(pressed);
                assert_eq!(time, 7);
            }
            other => panic!("expected PointerButton, got {other:?}"),
        }
        let key = s.map(
            InputEvent::KeyPress {
                source_id: TEST_SOURCE_ID,
                keycode: 30,
            },
            8,
        );
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
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::Button {
                source_id: TEST_SOURCE_ID,
                code: 1,
                pressed: true,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
            InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 1.0,
                dy: 1.0,
            },
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
        // Coalesced raw deltas must SUM (5 motions of dx=dy=1 → 5,5), not
        // collapse to the last one — else XI2 RawMotion loses distance and
        // SDL2 relative-mouse apps under-track. The producer position stays
        // fixed because KMS integrates physical relative deltas. (#96 follow-up)
        match &collected[0] {
            Message::HostInput(HostInputEvent::PointerMotion {
                x: 400,
                y: 300,
                dx: 5,
                dy: 5,
                motion_delta: Some([5.0, 5.0]),
                ..
            }) => {}
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
            Message::HostInput(HostInputEvent::PointerMotion {
                x: 400,
                y: 300,
                dx: 3,
                dy: 3,
                motion_delta: Some([3.0, 3.0]),
                ..
            }) => {}
            other => panic!("third message: {other:?}"),
        }
        // Silence unused warning on `poll` — we just need its waker
        // alive for the channel to function.
        drop(poll);
    }

    #[test]
    fn kms_pointer_authority_input_thread_preserves_fractional_relative_motion() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [0.4, 0.4, 0.4].map(|dx| InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx,
                dy: 0.0,
            }),
            100,
        )
        .unwrap();
        if let Some(motion) = pending.take() {
            sender.send(Message::HostInput(motion)).unwrap();
        }

        assert_eq!(state.cursor(), (400.0, 300.0));
        let messages: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            Message::HostInput(HostInputEvent::PointerMotion {
                origin,
                x,
                y,
                dx,
                dy,
                motion_delta,
                relative,
                ..
            }) => {
                assert_eq!(*origin, InputOrigin::Physical(TEST_SOURCE_ID));
                assert_eq!((*x, *y), (400, 300));
                assert_eq!((*dx, *dy), (0, 0));
                let Some([motion_dx, motion_dy]) = motion_delta else {
                    panic!("relative motion must retain the fractional delta");
                };
                assert!((*motion_dx - 1.2).abs() < 1e-12);
                assert_eq!(*motion_dy, 0.0);
                assert!(*relative);
            }
            other => panic!("expected a relative pointer motion, got {other:?}"),
        }
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
            [InputEvent::PointerMotion {
                source_id: TEST_SOURCE_ID,
                dx: 5.0,
                dy: 0.0,
            }],
            1,
        )
        .unwrap();
        assert!(pending.is_some());
        let immediate: Vec<Message> = rx.try_recv_all().collect();
        assert!(immediate.is_empty(), "no flush yet, got {immediate:?}");

        // Batch B: motion then button — only the latest combined
        // motion + button get sent, while deltas stay separate from x/y.
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::PointerMotion {
                    source_id: TEST_SOURCE_ID,
                    dx: 10.0,
                    dy: 0.0,
                },
                InputEvent::Button {
                    source_id: TEST_SOURCE_ID,
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
            Message::HostInput(HostInputEvent::PointerMotion {
                x: 400,
                y: 300,
                dx: 15,
                motion_delta: Some([15.0, 0.0]),
                ..
            }) => {}
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
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_BACKSPACE,
                },
                // Anything after the zap is dropped — the server is
                // already shutting down. This press must NOT reach
                // the core.
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
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
                source_id: TEST_SOURCE_ID,
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
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyRelease {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
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
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_RIGHTCTRL,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_RIGHTALT,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
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
    fn vt_pause_clears_hotkeys_without_forwarding_key_releases() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        let other_source = InputSourceId(2);
        state.hotkey.check(&InputEvent::KeyPress {
            source_id: other_source,
            keycode: LINUX_KEY_LEFTCTRL,
        });
        state.hotkey.check(&InputEvent::KeyPress {
            source_id: other_source,
            keycode: LINUX_KEY_LEFTALT,
        });

        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::DeviceSuspended {
                source_id: TEST_SOURCE_ID,
            }],
            0,
        )
        .unwrap();
        // This is the reset performed by the production Pause command after
        // forwarding the suspend lifecycle batch.
        reset_hotkeys_after_vt_pause(&mut state);

        assert_eq!(
            state.hotkey.check(&InputEvent::KeyPress {
                source_id: other_source,
                keycode: 60,
            }),
            None,
            "a bare F2 after VT pause must not inherit Ctrl+Alt",
        );
        let messages: Vec<_> = rx.try_recv_all().collect();
        assert_eq!(messages.len(), 1, "pause only forwards suspend lifecycle");
        assert!(matches!(
            messages.as_slice(),
            [Message::HostInput(HostInputEvent::DeviceSuspended { source_id })]
                if *source_id == TEST_SOURCE_ID
        ));
        assert!(state.scroll_accum_by_source.is_empty());
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
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
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
    /// for the per-drawable storage dump (Ctrl-Alt-F12 hotkey path).
    #[test]
    fn ctrl_alt_f12_emits_dump_drawables_and_drops_keypress() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    source_id: TEST_SOURCE_ID,
                    keycode: LINUX_KEY_F12,
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
                Message::HostInput(HostInputEvent::Key(ev)) if ev.pressed && u32::from(ev.keycode) == LINUX_KEY_F12 + 8
            )),
            "F12 keypress must not be forwarded after dump-drawables hotkey, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn input_thread_batch_gate() {
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
                source_id: TEST_SOURCE_ID,
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
                source_id: TEST_SOURCE_ID,
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
        state.drain_scroll(TEST_SOURCE_ID, 0, 60, 0, &mut out);
        assert!(out.is_empty(), "60 < 120, no emission yet");
        state.drain_scroll(TEST_SOURCE_ID, 0, 30, 0, &mut out);
        assert!(out.is_empty(), "60 + 30 = 90 < 120");
        state.drain_scroll(TEST_SOURCE_ID, 0, 40, 0, &mut out);
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
        state.drain_scroll(TEST_SOURCE_ID, 0, -120, 0, &mut out);
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
        state.drain_scroll(TEST_SOURCE_ID, 0, 480, 0, &mut out);
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
        state.drain_scroll(TEST_SOURCE_ID, 120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_RIGHT,
                pressed: true,
                ..
            }
        ));
        out.clear();
        state.drain_scroll(TEST_SOURCE_ID, -120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_LEFT,
                pressed: true,
                ..
            }
        ));
    }

    /// A config-only push latches no pause/resume command, so `drain`
    /// (which the loop calls first) returns `None` — but `take_configs`
    /// must still surface the queued write. This is the exact path the
    /// direct-mode `xinput set-prop` fix relies on: regression-guards
    /// against re-coupling config delivery to the command latch.
    #[test]
    fn config_push_survives_command_drain() {
        let control = InputThreadControl::new().expect("control");
        control.push_config(
            DeviceConfigToken(1),
            InputSourceId(2),
            DeviceConfigChange::NaturalScroll(false),
        );
        // No pause/resume was published, so the latched command is empty.
        assert!(control.drain().is_empty());
        // The config write is still pending and drains FIFO.
        let configs = control.take_configs();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, DeviceConfigToken(1));
        assert_eq!(configs[0].1, InputSourceId(2));
        assert!(matches!(
            configs[0].2,
            DeviceConfigChange::NaturalScroll(false)
        ));
        // Queue is emptied by the take.
        assert!(control.take_configs().is_empty());
    }

    /// Config writes and a pause/resume command can ride the same wakeup;
    /// both must be observable, and configs preserve enqueue order.
    #[test]
    fn config_queue_is_fifo_and_independent_of_command() {
        let control = InputThreadControl::new().expect("control");
        let source = InputSourceId(2);
        control.push_config(
            DeviceConfigToken(1),
            source,
            DeviceConfigChange::NaturalScroll(true),
        );
        control.push_config(DeviceConfigToken(2), source, DeviceConfigChange::Tap(false));
        control.pause();
        assert_eq!(control.drain(), vec![InputThreadCommand::Pause]);
        let configs = control.take_configs();
        assert_eq!(configs.len(), 2);
        assert!(matches!(
            configs[0].2,
            DeviceConfigChange::NaturalScroll(true)
        ));
        assert!(matches!(configs[1].2, DeviceConfigChange::Tap(false)));
    }

    #[test]
    fn xi_config_completion_reports_the_input_thread_apply_result() {
        let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
        let control = InputThreadControl::new().expect("control");
        let source = InputSourceId(91);
        let token = DeviceConfigToken(17);
        let change = DeviceConfigChange::AccelSpeed(0.5);
        control.push_config(token, source, change);
        let mut waiting = VecDeque::new();

        process_config_commands(
            control.take_configs(),
            &sender,
            &mut waiting,
            |actual_source, actual_change| {
                assert_eq!(actual_source, source);
                assert_eq!(actual_change, change);
                (Ok(()), false)
            },
        )
        .expect("send confirmed result");

        assert!(waiting.is_empty());
        assert!(matches!(
            receiver.try_recv_all().next(),
            Some(Message::DeviceConfigResult {
                token: actual_token,
                source: actual_source,
                result: Ok(()),
            }) if actual_token == token && actual_source == source
        ));
        assert!(receiver.try_recv_all().next().is_none());
    }

    #[test]
    fn submitted_config_waits_for_proven_resume_then_reports_success() {
        let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
        let source = InputSourceId(92);
        let token = DeviceConfigToken(18);
        let change = DeviceConfigChange::NaturalScroll(true);
        let mut waiting = VecDeque::new();

        process_config_commands(
            vec![(token, source, change)],
            &sender,
            &mut waiting,
            |_, _| (Err(DeviceConfigError::SourceGone), true),
        )
        .expect("queue the proven-continuation wait");
        assert_eq!(waiting, VecDeque::from([(token, source, change)]));
        assert!(receiver.try_recv_all().next().is_none());

        let resumed = resumed_info(source);
        service_waiting_device_configs(
            &sender,
            &mut waiting,
            &[InputEvent::DeviceResumed(resumed)],
            |actual_source, actual_change| {
                assert_eq!(actual_source, source);
                assert_eq!(actual_change, change);
                Ok(())
            },
        )
        .expect("apply to the proven rebind and report it");

        assert!(waiting.is_empty());
        assert!(matches!(
            receiver.try_recv_all().next(),
            Some(Message::DeviceConfigResult {
                token: actual_token,
                source: actual_source,
                result: Ok(()),
            }) if actual_token == token && actual_source == source
        ));
    }

    #[test]
    fn waiting_config_snapshot_keeps_resume_events_after_batch_consumption() {
        let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
        let source = InputSourceId(94);
        let token = DeviceConfigToken(20);
        let change = DeviceConfigChange::NaturalScroll(true);
        let events = vec![InputEvent::DeviceResumed(resumed_info(source))];
        let waiting = VecDeque::from([(token, source, change)]);

        let snapshot = snapshot_lifecycle_events_for_waiters(&waiting, &events)
            .expect("a pending config keeps the lifecycle events beside the consumed batch");
        assert!(matches!(
            snapshot.as_slice(),
            [InputEvent::DeviceResumed(info)] if info.source_id == source
        ));
        assert!(
            snapshot_lifecycle_events_for_waiters(&VecDeque::new(), &events).is_none(),
            "the common path without a waiting config needs no batch clone"
        );
        drop(events); // process_batch consumes the original input vector.

        let mut waiting = waiting;
        service_waiting_device_configs(
            &sender,
            &mut waiting,
            &snapshot,
            |actual_source, actual_change| {
                assert_eq!(actual_source, source);
                assert_eq!(actual_change, change);
                Ok(())
            },
        )
        .expect("resume lifecycle event retries the waiting config");

        assert!(waiting.is_empty());
        assert!(matches!(
            receiver.try_recv_all().next(),
            Some(Message::DeviceConfigResult {
                token: actual_token,
                source: actual_source,
                result: Ok(()),
            }) if actual_token == token && actual_source == source
        ));
    }

    #[test]
    fn submitted_config_expiring_without_rebind_reports_source_gone() {
        let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
        let source = InputSourceId(93);
        let token = DeviceConfigToken(19);
        let change = DeviceConfigChange::Tap(false);
        let mut waiting = VecDeque::new();

        process_config_commands(
            vec![(token, source, change)],
            &sender,
            &mut waiting,
            |_, _| (Err(DeviceConfigError::SourceGone), true),
        )
        .expect("queue the proven-continuation wait");
        assert_eq!(waiting.len(), 1);

        service_waiting_device_configs(
            &sender,
            &mut waiting,
            &[InputEvent::DeviceRemoved { source_id: source }],
            |_, _| panic!("removed source must not be applied"),
        )
        .expect("report source expiry");

        assert!(waiting.is_empty());
        assert!(matches!(
            receiver.try_recv_all().next(),
            Some(Message::DeviceConfigResult {
                token: actual_token,
                source: actual_source,
                result: Err(DeviceConfigError::SourceGone),
            }) if actual_token == token && actual_source == source
        ));
    }
}
