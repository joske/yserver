//! Rootless presentation: each top-level's backing reaches its native window
//! (spec `docs/superpowers/specs/2026-10-19-rootless-core-design.md` §4.5).
//! The surface platform's (macOS, Windows), and the card-less KMS platform's
//! for a host served on the core thread ([`display`], a Wayland
//! compositor): the presenter itself runs on any [`Platform`].

// Nothing outside tests starts the presenter until RL2 (macOS).
#![cfg_attr(not(test), allow(dead_code))]

mod cpu;
#[cfg(kms)]
pub(crate) mod display;
pub(crate) mod frames;
mod gpu;
pub(crate) mod host;
pub(crate) mod native;
#[cfg(kms)]
pub(crate) mod present_clock;
pub(crate) mod record;
pub(crate) mod window_swapchain;

#[cfg(test)]
mod tests;

use std::{collections::HashMap, time::Instant};

use yserver_core::{
    rootless::{CoordinatePolicy, RootlessError, RootlessEvent, ToplevelId},
    server::ServerState,
};

use self::{
    cpu::CpuState,
    frames::{Readiness, WindowFrames},
    gpu::{GpuState, GpuTick},
    host::{FrameEvent, HandoffKind, RootlessHost},
    window_swapchain::Retiring,
};
use crate::{
    kms::render::{Platform, XBackend, store::DrawableStore},
    surface::SurfacePlatform,
};

/// How a window's frames travel.
pub(crate) enum Path {
    Cpu(CpuState),
    Gpu(Box<GpuState>),
}

/// One mapped top-level the presenter delivers.
pub(crate) struct Presented {
    /// The X window's backend handle (`host_window_to_backing` key).
    pub host_xid: u32,
    pub depth: u8,
    pub frames: WindowFrames,
    pub path: Path,
    /// `frame_wanted` was sent in this Busy period (D7).
    pub wanted: bool,
}

impl Presented {
    /// A host event ends a GPU window's out-of-date backoff.
    fn host_event(&mut self) {
        if let Path::Gpu(gpu) = &mut self.path {
            gpu.host_event();
        }
    }
}

/// What the presenter did, for tests that check the order against the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Note {
    Added(ToplevelId),
    Removed(ToplevelId),
}

/// The rootless half of the surface platform's backend state.
pub(crate) struct Presenter {
    pub(crate) host: Box<dyn RootlessHost>,
    pub(crate) windows: HashMap<ToplevelId, Presented>,
    /// Readbacks started or finished, frames shown: tests of the wakeup rule.
    pub(crate) progress: u64,
    /// Swapchains and surfaces waiting for their presents to finish.
    pub(crate) retiring: Vec<Retiring>,
    /// Act as if the device had no swapchain maintenance1.
    #[cfg(test)]
    pub(crate) no_maintenance1: bool,
    #[cfg(test)]
    pub(crate) observer: Option<Box<dyn FnMut(Note)>>,
}

impl Presenter {
    pub(crate) fn new(host: Box<dyn RootlessHost>) -> Self {
        Self {
            host,
            windows: HashMap::new(),
            progress: 0,
            retiring: Vec::new(),
            #[cfg(test)]
            no_maintenance1: false,
            #[cfg(test)]
            observer: None,
        }
    }

    fn note(&mut self, note: Note) {
        #[cfg(test)]
        if let Some(observer) = self.observer.as_mut() {
            observer(note);
        }
        #[cfg(not(test))]
        let _ = note;
    }

    /// `Mapped` → a new entry, `Unmapped` → teardown; the host hears of the
    /// batch afterwards (`on_rootless_events`).
    pub(crate) fn apply_events(&mut self, state: &ServerState, events: &[RootlessEvent]) {
        for event in events {
            match event {
                RootlessEvent::Mapped {
                    id, window, depth, ..
                } => {
                    let Some(host_xid) = state
                        .resources
                        .window(*window)
                        .and_then(|w| w.host_xid)
                        .map(|h| h.as_raw())
                    else {
                        log::warn!(
                            "rootless: mapped window 0x{:x} has no backend window",
                            window.0
                        );
                        continue;
                    };
                    let path = match self.host.handoff(*id, *depth) {
                        HandoffKind::Dmabuf => {
                            log::warn!(
                                "rootless: dmabuf handoff is not implemented; using CPU frames"
                            );
                            Path::Cpu(CpuState::new())
                        }
                        HandoffKind::Cpu => Path::Cpu(CpuState::new()),
                        HandoffKind::VulkanSurface => Path::Gpu(Box::new(GpuState::new())),
                    };
                    self.windows.insert(
                        *id,
                        Presented {
                            host_xid,
                            depth: *depth,
                            frames: WindowFrames::new(0),
                            path,
                            wanted: false,
                        },
                    );
                    self.note(Note::Added(*id));
                }
                RootlessEvent::Unmapped { id } => {
                    if let Some(w) = self.windows.remove(id) {
                        match w.path {
                            // The engine keeps the staging buffer until the copy finished.
                            Path::Cpu(mut cpu) => drop(cpu.take_in_flight()),
                            // Never destroyed in place; the host frees its
                            // native window at `surface_released`.
                            Path::Gpu(gpu) => self.retiring.extend((*gpu).retire(*id)),
                        }
                        self.note(Note::Removed(*id));
                    }
                }
                _ => {}
            }
        }
    }

    /// Host → presenter events; an id that is not live is ignored (spec §4.2).
    pub(crate) fn apply_frame_events(
        &mut self,
        store: &mut DrawableStore,
        events: Vec<FrameEvent>,
    ) {
        for event in events {
            match event {
                FrameEvent::SetupDone { id } => {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.frames.on_setup_done();
                        w.host_event();
                    }
                }
                FrameEvent::Ready { id } => {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.frames.on_ready();
                        w.host_event();
                    }
                }
                FrameEvent::Recreated { id } => {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.frames.restart();
                        if let Path::Cpu(cpu) = &mut w.path {
                            cpu.restart();
                        }
                        w.wanted = false;
                        w.host_event();
                    }
                }
                FrameEvent::Delivered { id, seq } => {
                    let Some(w) = self.windows.get_mut(&id) else {
                        continue;
                    };
                    let delivery = w.frames.on_delivered(seq);
                    if let Path::Cpu(cpu) = &w.path {
                        cpu::ack(store, cpu.backing(), delivery.ack_epoch);
                    }
                    if delivery.first {
                        self.host.first_frame(id);
                    }
                }
                FrameEvent::Dropped { id, seq } => {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.frames.on_dropped(seq);
                        if let Path::Cpu(cpu) = &mut w.path {
                            cpu.on_dropped(&w.frames);
                        }
                    }
                }
                FrameEvent::Released { id, seq, pixels } => {
                    if let Some(Presented {
                        path: Path::Cpu(cpu),
                        ..
                    }) = self.windows.get_mut(&id)
                    {
                        cpu.on_released(seq, pixels);
                    }
                }
            }
        }
    }

    /// Per window: follow the backing's generation; CPU: finish a completed
    /// readback into a frame and start the next; GPU: present; else ask a
    /// Busy host for a frame. Never waits on the CPU path.
    pub(crate) fn tick<P: Platform>(&mut self, b: &mut XBackend<P>) {
        let Some(vk) = b.platform.gpu().vk().cloned() else {
            return;
        };
        self.poll_retiring();
        if self.windows.is_empty() {
            return;
        }
        // Recorded paint must be submitted, its damage in the store, before
        // any readiness or damage decision (compose.rs
        // `flush_before_damage_exposure`); `next_wakeup` then sees it too.
        b.flush_before_damage_exposure();
        #[cfg(test)]
        let maintenance1 = vk.swapchain_maintenance1 && !self.no_maintenance1;
        #[cfg(not(test))]
        let maintenance1 = vk.swapchain_maintenance1;
        for (id, w) in &mut self.windows {
            let backing = b
                .core
                .host_window_to_backing
                .get(&w.host_xid)
                .and_then(|p| b.store.lookup(p.as_raw()));
            let Some(backing) = backing else {
                continue;
            };
            if backing.as_u64() != w.frames.backing_gen() {
                w.frames.new_generation(backing.as_u64());
                match &mut w.path {
                    Path::Cpu(cpu) => cpu.new_generation(backing),
                    Path::Gpu(gpu) => gpu.new_generation(backing),
                }
            }
            if w.frames.readiness() != Readiness::Busy {
                w.wanted = false;
            }
            let has_work = match &mut w.path {
                Path::Cpu(cpu) => {
                    if cpu.complete(*id, w.depth, &mut w.frames, self.host.as_mut(), &vk) {
                        self.progress += 1;
                        if w.frames.readiness() != Readiness::Busy {
                            w.wanted = false;
                        }
                    }
                    if cpu.wants_frame(&w.frames, &b.store) {
                        let mut g = cpu::Gpu {
                            store: &mut b.store,
                            engine: &mut b.engine,
                            gpu: b.platform.gpu_mut(),
                        };
                        if cpu.start(&w.frames, &mut g) {
                            self.progress += 1;
                        }
                        continue;
                    }
                    cpu.has_work(&b.store)
                }
                Path::Gpu(gpu) => {
                    let mut t = GpuTick {
                        vk: &vk,
                        store: &mut b.store,
                        gpu: b.platform.gpu(),
                        maintenance1,
                        retiring: &mut self.retiring,
                    };
                    if gpu.tick(*id, w.depth, &mut w.frames, self.host.as_mut(), &mut t) {
                        self.progress += 1;
                    }
                    gpu.has_work(&b.store)
                }
            };
            if w.frames.readiness() == Readiness::Busy && !w.wanted && has_work {
                w.wanted = true;
                self.host.frame_wanted(*id);
            }
        }
    }

    /// Destroy what finished retiring; the host may free a native window
    /// whose surface went.
    pub(crate) fn poll_retiring(&mut self) {
        let mut released = Vec::new();
        self.retiring.retain_mut(|r| {
            let gone = r.try_destroy();
            if gone {
                released.extend(r.released());
            }
            !gone
        });
        for id in released {
            self.host.surface_released(id);
        }
    }

    /// The presenter's deadline (spec §4.1 "Wakeups"): `now` for a frame
    /// the tick would start, the 1 ms GPU poll (compose.rs, D6) while a
    /// readback is in flight or a swapchain retires, else none: a Busy host
    /// wakes the loop itself.
    pub(crate) fn next_wakeup(&self, store: &DrawableStore, now: Instant) -> Option<Instant> {
        let mut deadline = (!self.retiring.is_empty()).then(|| now + READBACK_POLL);
        for w in self.windows.values() {
            let (wants, in_flight) = match &w.path {
                Path::Cpu(cpu) => (cpu.wants_frame(&w.frames, store), cpu.in_flight()),
                Path::Gpu(gpu) => {
                    if let Some(at) = gpu.retry_at(&w.frames, store) {
                        deadline = Some(deadline.map_or(at, |d| d.min(at)));
                    }
                    (gpu.wants_frame(&w.frames, store, now), false)
                }
            };
            if wants {
                return Some(now);
            }
            if in_flight {
                let poll = now + READBACK_POLL;
                deadline = Some(deadline.map_or(poll, |d| d.min(poll)));
            }
        }
        deadline
    }
}

/// Readback completion is polled (D6), as polled Present batches are.
const READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(1);

impl XBackend<SurfacePlatform> {
    /// Retirement polls at the loop head too (`before_block`).
    pub(crate) fn rootless_before_block(&mut self) {
        if let Some(presenter) = self.pstate.rootless.as_mut() {
            presenter.poll_retiring();
        }
    }

    /// The presenter's share of `maybe_composite`, after the scene.
    pub(crate) fn rootless_tick(&mut self) {
        let Some(mut presenter) = self.pstate.rootless.take() else {
            return;
        };
        presenter.tick(self);
        self.pstate.rootless = Some(presenter);
    }
}

/// Install the presenter and turn the core's rootless mode on.
///
/// # Errors
///
/// What `yserver_core::rootless::enable` refuses; nothing is installed then.
pub(crate) fn start(
    state: &mut ServerState,
    b: &mut XBackend<SurfacePlatform>,
    host: Box<dyn RootlessHost>,
    policy: CoordinatePolicy,
) -> Result<(), RootlessError> {
    yserver_core::rootless::enable(state, b, policy)?;
    b.pstate.rootless = Some(Presenter::new(host));
    Ok(())
}
