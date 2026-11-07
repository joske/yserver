//! Rootless mode on the card-less KMS platform: a host served on the core
//! thread (`crate::rootless::display`) shows each top-level, and
//! the platform keeps what only it has -- DRI3 on a render node, MIT-SHM,
//! Present -- for the host's clients.
//!
//! The root takes the host's screen's size, and RandR lists that screen as
//! one connected output (the host names it), with the host's refresh, no provider
//! (Xwayland lists none: a provider that owns no CRTC sends Wine's X driver
//! dividing by a DPI of zero), and a Present clock of its own
//! ([`PresentClock`]). The presenter (`crate::rootless::Presenter`) is the
//! surface platform's, run here on the backing store and engine this
//! platform has.

use std::{io, time::Instant};

use yserver_core::{
    backend::Backend as _,
    core_loop::handle_host_input,
    rootless::{CoordinatePolicy, RootlessError, RootlessEvent},
    server::ServerState,
};

use crate::{
    kms::render::backend::KmsBackend,
    rootless::{
        Presenter,
        display::{HostItem, HostWork, PresenterHandle, SharedHost},
        host::FrameEvent,
        present_clock::{PresentClock, monotonic_us},
    },
};

/// What the platform keeps of a rootless host.
pub(crate) struct RootlessKms {
    pub(crate) host: SharedHost,
    /// `None` until [`KmsBackend::start_rootless`] enabled the core's mode.
    pub(crate) presenter: Option<Presenter>,
    /// The output's `(output, CRTC, mode)` ids, allocated on first
    /// projection so they stay the same afterwards.
    pub(crate) randr_ids: Option<(u32, u32, u32)>,
    pub(crate) clock: PresentClock,
    /// The cursor the host was last given: `(version, hotspot)`, or none;
    /// `None` before the first.
    cursor_sent: Option<Option<(u64, (u16, u16))>>,
}

impl KmsBackend {
    /// Become a rootless server for `host`, before the topology is read:
    /// the root takes the host's screen's size, and the keymap the host's
    /// keyboard layout.
    ///
    /// # Errors
    ///
    /// Resizing the virtual screen failed.
    #[cfg_attr(
        not(wayland_host),
        allow(dead_code, reason = "the Wayland host attaches itself with it")
    )]
    pub(crate) fn attach_rootless_host(&mut self, host: SharedHost) -> io::Result<()> {
        let (screen, layout) = {
            let host = host.borrow();
            (host.screen(), host.keyboard_layout())
        };
        if let Some((layout, variant)) = layout {
            let rmlvo = crate::kms::core::XkbRmlvo {
                layout: layout.clone(),
                variant: variant.clone(),
                ..self.core.xkb_rmlvo.clone()
            };
            if self.core.recompile_keymap(&rmlvo).is_some() {
                log::info!("rootless: the keyboard is the host's, {layout} {variant:?}");
            }
        }
        match screen {
            Some(screen) => {
                self.apply_virtual_screen_extent(screen.width, screen.height)?;
                log::info!(
                    "rootless: the root window is the host's screen, {}x{}",
                    screen.width,
                    screen.height
                );
            }
            None => log::warn!("rootless: the host described no screen; the root stays as it is"),
        }
        let refresh = screen.map_or(0, |screen| screen.refresh_mhz);
        let clock = PresentClock::new(monotonic_us(), refresh);
        log::info!(
            "rootless: the Present clock runs at {} ns an MSC, the host's {refresh} mHz",
            clock.period_ns()
        );
        self.pstate.rootless = Some(RootlessKms {
            host,
            presenter: None,
            randr_ids: None,
            clock,
            cursor_sent: None,
        });
        Ok(())
    }

    /// Turn the core's rootless mode on for the attached host, with the
    /// presenter delivering its windows.
    ///
    /// # Errors
    ///
    /// What `yserver_core::rootless::enable` refuses.
    #[cfg_attr(
        not(wayland_host),
        allow(dead_code, reason = "the Wayland host starts rootless mode with it")
    )]
    pub(crate) fn start_rootless(&mut self, state: &mut ServerState) -> Result<(), RootlessError> {
        let Some(host) = self.pstate.rootless.as_ref().map(|r| r.host.clone()) else {
            return Ok(());
        };
        yserver_core::rootless::enable(state, self, CoordinatePolicy::Opaque)?;
        if let Some(rootless) = self.pstate.rootless.as_mut() {
            rootless.presenter = Some(Presenter::new(Box::new(PresenterHandle(host))));
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn rootless_host_fd(&self) -> Option<std::os::fd::RawFd> {
        Some(self.pstate.rootless.as_ref()?.host.borrow().fd())
    }

    /// The host's connection is readable: read it, then apply what it said
    /// with the host no longer borrowed. `false` when it closed.
    pub(in crate::kms::render::backend) fn rootless_on_display_ready(
        &mut self,
        state: &mut ServerState,
    ) -> bool {
        let Some(host) = self.pstate.rootless.as_ref().map(|r| r.host.clone()) else {
            return true;
        };
        let mut work = HostWork::default();
        let read = host.borrow_mut().dispatch(&mut work);
        self.apply_host_work(state, work);
        host.borrow_mut().flush();
        match read {
            Ok(()) => true,
            Err(error) => {
                log::warn!("rootless: the host's display: {error}");
                false
            }
        }
    }

    /// Apply what the host said in the order it said it. Frame events
    /// that come together go to the presenter together.
    pub(in crate::kms::render::backend) fn apply_host_work(
        &mut self,
        state: &mut ServerState,
        work: HostWork,
    ) {
        let mut frames = Vec::new();
        for item in work {
            match item {
                HostItem::Frame(event) => frames.push(event),
                HostItem::Request(request) => {
                    self.apply_rootless_frames(std::mem::take(&mut frames));
                    yserver_core::rootless::host_request(state, self, request);
                }
                HostItem::Input(event) => {
                    self.apply_rootless_frames(std::mem::take(&mut frames));
                    handle_host_input(state, self, event);
                }
            }
        }
        self.apply_rootless_frames(frames);
    }

    fn apply_rootless_frames(&mut self, frames: Vec<FrameEvent>) {
        if frames.is_empty() {
            return;
        }
        if let Some(presenter) = self
            .pstate
            .rootless
            .as_mut()
            .and_then(|r| r.presenter.as_mut())
        {
            presenter.apply_frame_events(&mut self.store, frames);
        }
    }

    /// The core's events of this iteration: the presenter's first, then the
    /// host's.
    pub(in crate::kms::render::backend) fn rootless_events(
        &mut self,
        state: &ServerState,
        events: &[RootlessEvent],
    ) {
        if let Some(presenter) = self
            .pstate
            .rootless
            .as_mut()
            .and_then(|r| r.presenter.as_mut())
        {
            presenter.apply_events(state, events);
            presenter.host.on_events(events);
        }
    }

    /// Once per loop iteration, before the reconcile: the host's own work
    /// with the state in hand, then what it queued since its connection was
    /// last read.
    pub(in crate::kms::render::backend) fn rootless_service(&mut self, state: &mut ServerState) {
        let Some(host) = self.pstate.rootless.as_ref().map(|r| r.host.clone()) else {
            return;
        };
        host.borrow_mut().service(state, self);
        let mut work = HostWork::default();
        host.borrow_mut().take_queued(&mut work);
        self.apply_host_work(state, work);
    }

    /// The presenter's share of `maybe_composite`, then the cursor, and
    /// what was queued for the host sent.
    pub(in crate::kms::render::backend) fn rootless_tick(&mut self) {
        let Some(mut rootless) = self.pstate.rootless.take() else {
            return;
        };
        let now = monotonic_us();
        rootless.clock.expire(now);
        let refresh = rootless.host.borrow().screen().map_or(0, |s| s.refresh_mhz);
        rootless.clock.set_refresh(now, refresh);
        if let Some(presenter) = rootless.presenter.as_mut() {
            presenter.poll_retiring();
            presenter.tick(self);
            self.flush_rootless_cursor(&mut rootless);
        }
        rootless.host.borrow_mut().flush();
        self.pstate.rootless = Some(rootless);
    }

    /// The host has no cursor plane: it gets the sprite itself, its bytes
    /// copied only when it changed.
    fn flush_rootless_cursor(&self, rootless: &mut RootlessKms) {
        let wanted = self.sprite_key();
        if rootless.cursor_sent == Some(wanted) {
            return;
        }
        let image = match wanted {
            None => None,
            Some(_) => match self.get_active_cursor_image() {
                Some(image) => Some(image),
                None => return,
            },
        };
        rootless.cursor_sent = Some(wanted);
        if let Some(presenter) = rootless.presenter.as_mut() {
            presenter.host.set_cursor(image.as_ref());
        }
    }

    /// The presenter's and the host's deadline, and the Present clock's
    /// next MSC while Present work waits on it.
    pub(in crate::kms::render::backend) fn rootless_next_wakeup(&self) -> Option<Instant> {
        let rootless = self.pstate.rootless.as_ref()?;
        let now = Instant::now();
        let presenter = rootless
            .presenter
            .as_ref()
            .and_then(|p| p.next_wakeup(&self.store, now));
        let clock = rootless.clock.wake().map(|wake| {
            now + std::time::Duration::from_micros(wake.saturating_sub(monotonic_us()))
        });
        let host = rootless.host.borrow().next_wakeup();
        [presenter, clock, host].into_iter().flatten().min()
    }

    pub(in crate::kms::render::backend) fn rootless_active(&self) -> bool {
        self.pstate.rootless.is_some()
    }

    /// Whether `crtc_id` is the host output's CRTC, whose Present clock is
    /// [`PresentClock`] rather than a kernel's.
    pub(in crate::kms::render::backend) fn is_rootless_present_crtc(&self, crtc_id: u32) -> bool {
        crtc_id != 0
            && self
                .pstate
                .rootless
                .as_ref()
                .and_then(|r| r.randr_ids)
                .is_some_and(|(_, crtc, _)| crtc == crtc_id)
    }

    /// The host output's `(msc, ust)` now, for `crtc_id` when it is the
    /// host output's.
    pub(in crate::kms::render::backend) fn rootless_present_clock(
        &self,
        crtc_id: u32,
    ) -> Option<(u64, u64)> {
        if !self.is_rootless_present_crtc(crtc_id) {
            return None;
        }
        Some(self.pstate.rootless.as_ref()?.clock.sample(monotonic_us()))
    }

    /// Have the loop come round at the host output's next MSC: `Some`
    /// whether a wake was newly armed, for `crtc_id` when it is the host
    /// output's.
    pub(in crate::kms::render::backend) fn arm_rootless_present_clock(
        &mut self,
        crtc_id: u32,
    ) -> Option<bool> {
        if !self.is_rootless_present_crtc(crtc_id) {
            return None;
        }
        Some(self.pstate.rootless.as_mut()?.clock.arm(monotonic_us()))
    }

    /// The rootless RandR topology: one connected output covering the root,
    /// at the host's refresh, not a fixed 60 (Chromium paces its frames by
    /// the RandR mode's). The X screen's size comes from the outputs, so
    /// without it the root would stay 0×0.
    pub(in crate::kms::render::backend) fn rootless_randr_outputs_and_modes(
        &mut self,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        let Some(rootless) = self.pstate.rootless.as_mut() else {
            return (Vec::new(), Vec::new());
        };
        let alloc = &mut self.pstate.randr_id_alloc;
        let (output_id, crtc_id, mode_id) = *rootless
            .randr_ids
            .get_or_insert_with(|| (alloc.fresh(), alloc.fresh(), alloc.fresh()));
        let (refresh, name) = {
            let host = rootless.host.borrow();
            (
                host.screen().map_or(0, |s| s.refresh_mhz),
                host.output_name(),
            )
        };
        let vrefresh = u32::try_from((refresh + 500) / 1000)
            .ok()
            .filter(|&hz| hz > 0)
            .unwrap_or(60);
        let (width, height) = self.platform.fb_dimensions();
        let outputs = vec![yserver_core::randr::RandrOutput {
            name: name.to_string(),
            output_id,
            crtc_id,
            mode_id,
            connected: true,
            x: 0,
            y: 0,
            width,
            height,
            vrefresh,
            timing: None,
            // No EDID: `RandrState::output_info` synthesises 96 DPI.
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![mode_id],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: yserver_core::randr::RR_ROTATE_0,
        }];
        let modes = yserver_core::randr::RandrState::from_outputs(0, outputs.clone()).mode_table;
        (outputs, modes)
    }

    /// The host output's `(output, CRTC)` association, which is not among
    /// `randr_id_alloc`'s entries: without it the first rebuild drops it,
    /// GetOutputInfo reports crtc 0 (the output looks disabled), and a
    /// client that builds its display list from RandR finds no screen
    /// (Chromium: "CreateOutputWindow: Could not find display info").
    pub(in crate::kms::render::backend) fn rootless_crtc_association(&self) -> Option<(u32, u32)> {
        let (output, crtc, _) = self.pstate.rootless.as_ref()?.randr_ids?;
        Some((output, crtc))
    }
}
