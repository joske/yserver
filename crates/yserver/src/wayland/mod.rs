//! The rootless Wayland host: each top-level X window becomes an
//! `xdg_toplevel` of the compositor `WAYLAND_DISPLAY` names, and each
//! override-redirect one an `xdg_popup`.
//!
//! With `--rootless` the server is a client of a Wayland
//! compositor rather than the owner of a DRM card: it runs on the card-less
//! KMS platform (`kms/render/backend/kms/rootless.rs`), which keeps DRI3 on
//! a render node and Present for its clients, its root takes the size of
//! the compositor's first screen, and the core's rootless mode
//! (`yserver_core::rootless`) says which windows the compositor shows. The
//! Wayland side is `wayland-client` on its own Rust implementation of the
//! wire ([`client`]), served on the core thread: its socket is polled as
//! `BackendFdKind::HostDisplay`, and each readiness is one non-blocking
//! dispatch (`crate::rootless::display`).
//!
//! # Windows
//!
//! A `Mapped` event makes the compositor's window, with the title, app id
//! (`WM_CLASS`'s class), parent (`WM_TRANSIENT_FOR`) and size limits
//! (`WM_NORMAL_HINTS`) set before its first commit, where a compositor
//! decides whether it floats; `InfoChanged` follows them. The compositor's
//! first `configure` ends the setup (`FrameEvent::SetupDone`), a later one
//! with another size is `HostRequest::Resized`, and its `close` is
//! `HostRequest::CloseRequested`. An override-redirect window is a popup
//! placed on its parent at `O(popup) − O(parent)`, the parent the core
//! chose, else the window the pointer is on, else the newest window. A
//! client's `_NET_WM_MOVERESIZE` is the compositor's drag of the window
//! (`xdg_toplevel.move` or `resize`).
//!
//! The presenter hands each window's backing over as an owned CPU frame
//! (`HandoffKind::Cpu`): X's 32-bit `ZPixmap` is `wl_shm`'s layout,
//! `XRGB8888` below depth 32 and premultiplied `ARGB8888` at it, so the
//! frame is copied into the compositor's buffer as it is, damaged where it
//! changed since the last frame shown. A window is paced by its frame
//! callbacks (`FrameEvent::Ready`).
//!
//! A top-level is shown without its X border, which the compositor's frame
//! takes the place of, as a reparenting window manager's would: its native
//! window is its content, the backing cropped by the border (and pointer
//! positions and the compositor's sizes moved by it). An override-redirect
//! window is shown whole, border included, since no window manager frames
//! it.
//!
//! # The rest
//!
//! The seat (keys, pointer, wheel, focus, the cursor and a game's pointer
//! lock) is [`input`]'s, and the clipboard, X's `CLIPBOARD` and `PRIMARY`
//! both ways, [`clipboard`]'s.

use std::{
    collections::HashMap,
    io,
    os::fd::RawFd,
    time::{Duration, Instant},
};

use yserver_core::{
    backend::{ActiveCursorImage, Backend},
    rootless::{
        Geometry, HostOutput, HostRequest, HostSerial, InfoFields, ResizeEdge, RootlessEvent,
        ToplevelId, ToplevelKind, WindowInfo, WmRequest,
    },
    server::ServerState,
};

use crate::{
    kms::vk::device::VkContext,
    rootless::{
        display::{DisplayHost, HostKeymap, HostScreen, HostWork},
        host::{CpuFrame, FrameEvent, HandoffKind, RootlessHost},
    },
};

mod client;
mod clipboard;
mod input;

use client::{Client, Event, Placement, Rect, SurfaceId, ToplevelOptions};

/// How long a window the compositor refused to make waits before it is
/// tried again.
const REFUSED_RETRY: Duration = Duration::from_secs(1);

/// What the compositor's window or popup for a top-level was made from.
#[derive(Clone, Debug)]
struct Spec {
    kind: ToplevelKind,
    geometry: Geometry,
    info: WindowInfo,
}

/// A top-level as the compositor's window or popup.
#[derive(Debug)]
struct Win {
    spec: Spec,
    /// `None` while the compositor has not made one (refused, or a popup
    /// waiting for its parent, or one the compositor took away).
    surface: Option<SurfaceId>,
    /// The compositor's first configure came: the setup is done.
    configured: bool,
    /// Its first frame was delivered: the compositor has it mapped, and a
    /// popup can hang from it.
    shown: bool,
    /// When to try making the window again after a refusal.
    retry_at: Option<Instant>,
    /// When to ask for its frame again after one that could not be drawn.
    redraw_at: Option<Instant>,
    /// A popup the compositor took away (`popup_done`): not made again
    /// until its X window unmaps.
    dismissed: bool,
    /// The popup's parent and placement it was made with.
    placed: Option<(ToplevelId, Placement)>,
    /// The window the compositor has as this window's parent
    /// (`xdg_toplevel.set_parent`).
    parent: Option<ToplevelId>,
}

/// The connection, what the compositor said about its screens, and the
/// windows made on it.
pub(crate) struct WaylandHost {
    client: Client,
    windows: HashMap<ToplevelId, Win>,
    /// Top-levels in the order they were mapped: a popup without a parent
    /// hangs from the newest.
    order: Vec<ToplevelId>,
    seat: input::Seat,
    clipboard: Option<clipboard::Clipboard>,
    /// What the compositor said, waiting for the core.
    work: HostWork,
    next_serial: u64,
    /// The screen the root was last given: at start, or by a
    /// `HostRequest::ScreenLayout`.
    screen_said: Option<HostScreen>,
}

impl WaylandHost {
    /// Connect to the compositor `WAYLAND_DISPLAY` names, and wait until it
    /// has described its screens and the keyboard's layout.
    ///
    /// # Errors
    ///
    /// No `WAYLAND_DISPLAY`, a socket that refuses, or a compositor that
    /// closes the connection during the first round trips.
    pub(crate) fn connect() -> io::Result<Self> {
        let client = Client::connect().map_err(|error| {
            io::Error::new(io::ErrorKind::NotConnected, format!("wayland: {error}"))
        })?;
        let clipboard = clipboard::Clipboard::bind(&client);
        if clipboard.is_none() {
            log::warn!("wayland: the compositor has no ext_data_control_v1; no clipboard");
        }
        let mut seat = input::Seat::default();
        seat.bind(&client);
        let mut host = Self {
            client,
            windows: HashMap::new(),
            order: Vec::new(),
            seat,
            clipboard,
            work: HostWork::default(),
            next_serial: 1,
            screen_said: None,
        };
        // The seat's devices, before its first input.
        for device in input::announcements() {
            host.work.input(device);
        }
        if host.client.keymap().is_none() {
            // The keymap comes after the seat's keyboard is made, which the
            // first round trips only start.
            match host.client.roundtrip() {
                Ok(events) => {
                    for event in events {
                        host.handle(event);
                    }
                }
                Err(error) => log::warn!("wayland: waiting for the keymap: {error}"),
            }
        }
        host.screen_said = host.screen();
        Ok(host)
    }

    /// The compositor's screen changed size since the root was given it:
    /// the root follows (`HostRequest::ScreenLayout`), as a window shell's
    /// does its window.
    fn follow_screen(&mut self) {
        let now = self.screen();
        if let Some(request) = screen_layout(self.screen_said, now) {
            log::info!(
                "wayland: the compositor's screen is {:?} now, the root follows",
                now.map(|screen| (screen.width, screen.height))
            );
            self.work.request(request);
            self.screen_said = now;
        }
    }

    /// The window manager's name: the compositor's process's, as the
    /// socket's peer names it, else `YSERVER_WM_NAME`, else none.
    pub(crate) fn window_manager_name(&self) -> Option<String> {
        peer_name(self.client.as_raw_fd()).or_else(|| std::env::var("YSERVER_WM_NAME").ok())
    }

    fn serial(&mut self) -> HostSerial {
        let serial = HostSerial(self.next_serial);
        self.next_serial += 1;
        serial
    }

    fn id_of(&self, surface: SurfaceId) -> Option<ToplevelId> {
        self.windows
            .iter()
            .find(|(_, win)| win.surface == Some(surface))
            .map(|(&id, _)| id)
    }

    fn handle(&mut self, event: Event) {
        let Some(event) = self.seat.take(event) else {
            return;
        };
        // What the seat read before this goes first: one queue, in the
        // order the compositor said it.
        self.deliver_seat();
        match event {
            Event::Frame(surface) => {
                if let Some(id) = self.id_of(surface) {
                    self.work.frame(FrameEvent::Ready { id });
                }
            }
            Event::Configure {
                surface,
                width,
                height,
            } => self.configured(surface, width, height),
            Event::Closed(surface) => {
                // A popup the compositor took away.
                if let Some(id) = self.id_of(surface)
                    && let Some(win) = self.windows.get_mut(&id)
                {
                    log::info!("wayland: the compositor took popup {id:?} away");
                    win.dismissed = true;
                    win.surface = None;
                    win.configured = false;
                    win.shown = false;
                    self.client.destroy(surface);
                }
            }
            Event::CloseRequested(surface) => {
                if let Some(id) = self.id_of(surface) {
                    log::info!("wayland: the compositor closes window {id:?}");
                    self.work.request(HostRequest::CloseRequested { id });
                }
            }
            Event::Clipboard(event) => {
                if let Some(clipboard) = self.clipboard.as_mut() {
                    clipboard.event(event);
                }
            }
            other => log::debug!("wayland: {other:?}"),
        }
    }

    /// The compositor's configure: the first ends the window's setup, a
    /// later one with a size it does not have asks X to resize it.
    fn configured(&mut self, surface: SurfaceId, width: u32, height: u32) {
        let Some(id) = self.id_of(surface) else {
            return;
        };
        let serial = self.serial();
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        if !win.configured {
            win.configured = true;
            self.work.frame(FrameEvent::SetupDone { id });
        }
        if win.spec.kind != ToplevelKind::Managed || width == 0 || height == 0 {
            return;
        }
        let shown = shown_size(&win.spec);
        let asked = (
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
        );
        if asked != shown {
            // The core takes the native content as the backing's extent.
            let border = crop(&win.spec).saturating_mul(2);
            self.work.request(HostRequest::Resized {
                id,
                width: asked.0.saturating_add(border),
                height: asked.1.saturating_add(border),
                serial,
            });
        }
    }

    /// Make the compositor's window or popup for `id`, when it has none.
    fn make(&mut self, id: ToplevelId) {
        let Some(win) = self.windows.get(&id) else {
            return;
        };
        if win.surface.is_some() || win.dismissed {
            return;
        }
        let spec = win.spec.clone();
        let made = match spec.kind {
            ToplevelKind::Managed => {
                let parent = self.parent_for(id, spec.info.transient_for);
                let (min_size, max_size) = size_limits(&spec.info);
                let options = ToplevelOptions {
                    title: title(&spec.info),
                    app_id: app_id(&spec.info),
                    size: size_u32(shown_size(&spec)),
                    parent: parent.map(|(_, surface)| surface),
                    min_size,
                    max_size,
                };
                self.client
                    .toplevel(&options)
                    .map(|surface| (surface, None))
                    .inspect(|_| {
                        if let Some(win) = self.windows.get_mut(&id) {
                            win.parent = parent.map(|(parent, _)| parent);
                        }
                    })
            }
            ToplevelKind::Popup { parent } => {
                let Some((parent, options)) = self.popup_placement(id, parent, &spec.geometry)
                else {
                    log::debug!("wayland: popup {id:?} has no window to hang from yet");
                    return;
                };
                let Some(parent_surface) = self.windows.get(&parent).and_then(|w| w.surface) else {
                    return;
                };
                self.client
                    .popup(parent_surface, &options)
                    .map(|surface| (surface, Some((parent, options))))
            }
        };
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        match made {
            Ok((surface, placed)) => {
                if let Some((parent, options)) = placed {
                    log::info!(
                        "wayland: override-redirect window {} ({}x{}) is a popup on {} at ({}, {})",
                        id.get(),
                        options.size.0,
                        options.size.1,
                        parent.get(),
                        options.anchor_rect.x,
                        options.anchor_rect.y,
                    );
                }
                log::info!(
                    "wayland: window {id:?} ({}x{}) is the compositor's {}{surface:?}, {:?} of {:?}",
                    spec.geometry.width,
                    spec.geometry.height,
                    if placed.is_some() { "popup " } else { "" },
                    title(&spec.info),
                    app_id(&spec.info),
                );
                win.surface = Some(surface);
                win.placed = placed;
                win.retry_at = None;
            }
            Err(error) => {
                // Said once, and tried again in a second: on every pass it
                // was 112 refusals in a minute.
                if win.retry_at.is_none() {
                    log::warn!("wayland: no window for {id:?}: {error}");
                }
                win.retry_at = Some(Instant::now() + REFUSED_RETRY);
            }
        }
    }

    /// A popup's parent and placement: the core's parent, else the window
    /// the pointer is on, else the newest window, any of them shown already
    /// (xdg-shell has a popup's parent mapped before the popup, and a
    /// compositor never shows one made earlier); placed at its outer corner
    /// relative to the parent's (spec §4.0).
    fn popup_placement(
        &self,
        id: ToplevelId,
        parent: Option<ToplevelId>,
        geometry: &Geometry,
    ) -> Option<(ToplevelId, Placement)> {
        let made = |candidate: &ToplevelId| {
            *candidate != id
                && self
                    .windows
                    .get(candidate)
                    .is_some_and(|w| w.surface.is_some() && w.shown)
        };
        let parent = parent
            .filter(made)
            .or_else(|| {
                self.seat
                    .pointer_surface()
                    .and_then(|s| self.id_of(s))
                    .filter(made)
            })
            .or_else(|| self.order.iter().rev().copied().find(made))?;
        let (left, top) = shown_origin(&self.windows.get(&parent)?.spec);
        let (width, height) = outer_size(geometry);
        Some((
            parent,
            // Top left, growing to the bottom right: exactly where X put
            // it, which the compositor must not move.
            Placement {
                size: (i32::from(width), i32::from(height)),
                anchor_rect: Rect {
                    x: geometry.x - left,
                    y: geometry.y - top,
                    width: 1,
                    height: 1,
                },
            },
        ))
    }

    fn mapped(&mut self, id: ToplevelId, spec: Spec) {
        self.windows.insert(
            id,
            Win {
                spec,
                surface: None,
                configured: false,
                shown: false,
                retry_at: None,
                redraw_at: None,
                dismissed: false,
                placed: None,
                parent: None,
            },
        );
        self.order.push(id);
        self.make(id);
    }

    fn unmapped(&mut self, id: ToplevelId) {
        self.order.retain(|other| *other != id);
        if let Some(win) = self.windows.remove(&id) {
            // Its dialogs are its parent's now, as xdg-shell has the
            // compositor take them.
            for other in self.windows.values_mut() {
                if other.parent == Some(id) {
                    other.parent = win.parent;
                }
            }
            self.seat.forget(&self.client, id);
            if let Some(surface) = win.surface {
                log::info!("wayland: window {id:?} is gone from the compositor");
                self.client.destroy(surface);
            }
        }
    }

    fn info_changed(&mut self, id: ToplevelId, info: WindowInfo, changed: InfoFields) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        win.spec.info = info;
        let Some(surface) = win.surface else {
            return;
        };
        if changed.contains(InfoFields::TITLE) {
            self.client.set_title(surface, &title(&win.spec.info));
        }
        if changed.contains(InfoFields::CLASS) {
            self.client.set_app_id(surface, &app_id(&win.spec.info));
        }
        if changed.contains(InfoFields::SIZE_HINTS) {
            let (min, max) = size_limits(&win.spec.info);
            self.client.set_size_limits(surface, min, max);
        }
        if changed.contains(InfoFields::TRANSIENT_FOR) && win.spec.kind == ToplevelKind::Managed {
            let transient_for = win.spec.info.transient_for;
            let parent = self.parent_for(id, transient_for);
            self.client
                .set_parent(surface, parent.map(|(_, surface)| surface));
            if let Some(win) = self.windows.get_mut(&id) {
                win.parent = parent.map(|(parent, _)| parent);
            }
        }
    }

    /// The compositor's window to make `id` a dialog of for its
    /// `WM_TRANSIENT_FOR`: a window made already, and none that would make a
    /// cycle of parents (itself, or a window that has `id` above it), which
    /// the compositor takes as a protocol error (`invalid_parent`) and ends
    /// the connection for.
    fn parent_for(
        &self,
        id: ToplevelId,
        transient_for: Option<ToplevelId>,
    ) -> Option<(ToplevelId, SurfaceId)> {
        let parent = transient_for?;
        let win = self.windows.get(&parent)?;
        if win.spec.kind != ToplevelKind::Managed {
            return None;
        }
        let surface = win.surface?;
        if makes_cycle(id, parent, |of| {
            self.windows.get(&of).and_then(|w| w.parent)
        }) {
            log::info!("wayland: window {id:?} is not made a dialog of {parent:?}: a cycle");
            return None;
        }
        Some((parent, surface))
    }

    fn configured_by_x(&mut self, id: ToplevelId, geometry: Geometry) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        win.spec.geometry = geometry;
        if !matches!(win.spec.kind, ToplevelKind::Popup { .. }) {
            // A window that resized itself has that size now: the next
            // configure that leaves the size to it must not resize it back.
            if let Some(surface) = win.surface {
                let size = size_u32(shown_size(&win.spec));
                self.client.resized_by_x(surface, size);
            }
            return;
        }
        // A popup follows where X puts it.
        let (Some(surface), Some((parent, placed))) = (win.surface, win.placed) else {
            return;
        };
        let parent_kind = win.spec.kind;
        let ToplevelKind::Popup {
            parent: core_parent,
        } = parent_kind
        else {
            return;
        };
        let Some((new_parent, options)) =
            self.popup_placement(id, core_parent.or(Some(parent)), &geometry)
        else {
            return;
        };
        if new_parent == parent && options != placed {
            let moved = self.client.reposition_popup(surface, &options);
            if let Some(win) = self.windows.get_mut(&id) {
                win.placed = Some((parent, options));
                let remade = moved.unwrap_or_else(|error| {
                    // Destroyed and not made again: tried again later, as a
                    // popup the compositor refused.
                    log::warn!("wayland: moving popup {id:?}: {error}");
                    win.surface = None;
                    win.placed = None;
                    win.retry_at = Some(Instant::now() + REFUSED_RETRY);
                    true
                });
                if remade {
                    // A new surface: shown once it is configured, and given
                    // a whole frame then.
                    win.configured = false;
                    win.shown = false;
                    self.work.frame(FrameEvent::Recreated { id });
                }
            }
        }
    }

    fn parent_changed(&mut self, id: ToplevelId, parent: Option<ToplevelId>) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        win.spec.kind = ToplevelKind::Popup { parent };
        // A popup's parent is fixed in xdg-shell: made again on the new one,
        // shown once it is configured and given a whole frame then.
        if let Some(surface) = win.surface.take() {
            win.configured = false;
            win.shown = false;
            win.placed = None;
            self.client.destroy(surface);
            self.work.frame(FrameEvent::Recreated { id });
            self.make(id);
        }
    }

    fn wm_request(&mut self, id: ToplevelId, request: WmRequest) {
        // `_NET_ACTIVE_WINDOW` for the window the compositor's keyboard is
        // on is granted, as the compositor's XWM grants it under Xwayland:
        // the window is activated again, `WM_TAKE_FOCUS` included. Wine
        // asks this after a window of its own closes, and its game takes
        // input only once it is answered. Another window's request is the
        // compositor's to grant, and is not carried out.
        if activation_granted(request, self.seat.keyboard_window(), id) {
            self.work.request(HostRequest::Activated { id });
            return;
        }
        let Some(surface) = self.windows.get(&id).and_then(|w| w.surface) else {
            return;
        };
        let edges = match request {
            WmRequest::BeginMove { .. } => 0,
            WmRequest::BeginResize { edge, .. } => xdg_edges(edge),
            other => {
                log::debug!("wayland: {other:?} for {id:?} not carried out");
                return;
            }
        };
        log::info!(
            "wayland: window {id:?} asks to be {} by the pointer",
            if edges == 0 { "moved" } else { "resized" }
        );
        self.client.drag_window(surface, edges);
    }
}

/// The root's new layout when the screen `now` is not the size it was
/// given (`said`): one output at the origin, the compositor's first screen.
fn screen_layout(said: Option<HostScreen>, now: Option<HostScreen>) -> Option<HostRequest> {
    let now = now?;
    let size = |screen: HostScreen| (screen.width, screen.height);
    (said.map(size) != Some(size(now))).then(|| HostRequest::ScreenLayout {
        outputs: vec![HostOutput {
            x: 0,
            y: 0,
            width: now.width,
            height: now.height,
        }],
    })
}

/// Whether a WM request is an activation the host grants itself: one for
/// the window the compositor's keyboard is on ([`WaylandHost::wm_request`]).
fn activation_granted(request: WmRequest, keyboard_on: Option<ToplevelId>, id: ToplevelId) -> bool {
    request == WmRequest::Activate && keyboard_on == Some(id)
}

impl RootlessHost for WaylandHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> HandoffKind {
        HandoffKind::Cpu
    }

    fn create_surface(
        &mut self,
        _id: ToplevelId,
        _vk: &VkContext,
    ) -> io::Result<ash::vk::SurfaceKHR> {
        Err(io::Error::other("the Wayland host takes CPU frames"))
    }

    /// Copy the frame into the compositor's buffer and commit it, damaged
    /// where it changed since the last frame shown; the bytes go straight
    /// back to the presenter.
    fn submit_cpu(&mut self, frame: CpuFrame) {
        let CpuFrame {
            id,
            seq,
            width,
            height,
            depth,
            pixels,
            damage,
            ..
        } = frame;
        let shown = self.windows.get(&id).filter(|win| win.configured);
        let surface = shown.and_then(|win| win.surface);
        let border = shown.map_or(0, |win| usize::from(crop(&win.spec)));
        let (width, height) = (usize::from(width), usize::from(height));
        let (shown_w, shown_h) = (
            width.saturating_sub(2 * border),
            height.saturating_sub(2 * border),
        );
        let delivered = surface.is_some_and(|surface| {
            match self.client.draw_pixels(
                surface,
                (
                    u32::try_from(shown_w).unwrap_or(0),
                    u32::try_from(shown_h).unwrap_or(0),
                ),
                depth != 32,
                |out| {
                    copy_cropped(&pixels, width, border, (shown_w, shown_h), out);
                    Some(cropped_damage(&damage, border, (shown_w, shown_h)))
                },
            ) {
                Ok(drawn) => {
                    if drawn {
                        self.client.commit_frame(surface);
                    }
                    drawn
                }
                Err(error) => {
                    log::warn!("wayland: drawing {id:?}: {error}");
                    false
                }
            }
        });
        let work = &mut self.work;
        if delivered {
            work.frame(FrameEvent::Delivered { id, seq });
        } else {
            work.frame(FrameEvent::Dropped { id, seq });
            // Liveness (spec §4.5.1): no frame callback was asked for, so
            // the retry is ours to start, once the window can take one. Not
            // at once: what could not be drawn now (no shared memory, no
            // descriptor) cannot be a loop iteration later either, and the
            // loop would read the window back and fail again without rest.
            if surface.is_some()
                && let Some(win) = self.windows.get_mut(&id)
            {
                win.redraw_at = Some(Instant::now() + REFUSED_RETRY);
            }
        }
        work.frame(FrameEvent::Released { id, seq, pixels });
    }

    /// The first commit mapped the surface: popups waiting for a window to
    /// hang from have one now.
    fn first_frame(&mut self, id: ToplevelId) {
        if let Some(win) = self.windows.get_mut(&id) {
            win.shown = true;
            self.make_waiting();
        }
    }

    fn on_events(&mut self, events: &[RootlessEvent]) {
        for event in events {
            match event {
                RootlessEvent::Mapped {
                    id,
                    kind,
                    geometry,
                    info,
                    ..
                } => self.mapped(
                    *id,
                    Spec {
                        kind: *kind,
                        geometry: *geometry,
                        info: info.clone(),
                    },
                ),
                RootlessEvent::Unmapped { id } => self.unmapped(*id),
                RootlessEvent::Configured { id, geometry, .. } => {
                    self.configured_by_x(*id, *geometry);
                }
                RootlessEvent::InfoChanged { id, info, changed } => {
                    self.info_changed(*id, info.clone(), *changed);
                }
                RootlessEvent::ParentChanged { id, parent } => self.parent_changed(*id, *parent),
                RootlessEvent::WmRequest { id, request } => self.wm_request(*id, *request),
                RootlessEvent::PointerLock { id } => {
                    let surface = id.and_then(|id| Some((id, self.windows.get(&id)?.surface?)));
                    self.seat.lock(&self.client, surface);
                }
                RootlessEvent::Restacked { .. }
                | RootlessEvent::Reshaped { .. }
                | RootlessEvent::Destroyed { .. }
                | RootlessEvent::ScreenChanged { .. } => {}
            }
        }
        // Popups whose parent went in this batch, on another window.
        self.make_waiting();
    }

    fn set_cursor(&mut self, cursor: Option<&ActiveCursorImage>) {
        self.seat.set_cursor(&mut self.client, cursor);
    }
}

impl DisplayHost for WaylandHost {
    fn fd(&self) -> RawFd {
        self.client.as_raw_fd()
    }

    fn wake_fd(&self) -> Option<RawFd> {
        self.clipboard.as_ref()?.wake_fd()
    }

    fn dispatch(&mut self, work: &mut HostWork) -> io::Result<()> {
        let read = match self.client.dispatch() {
            Ok(events) => {
                for event in events {
                    self.handle(event);
                }
                Ok(())
            }
            Err(error) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("{error}"),
            )),
        };
        self.deliver_seat();
        self.follow_screen();
        work.append(std::mem::take(&mut self.work));
        read
    }

    fn take_queued(&mut self, work: &mut HostWork) {
        work.append(std::mem::take(&mut self.work));
    }

    fn service(&mut self, state: &mut ServerState, backend: &mut dyn Backend) {
        // Windows the compositor refused, a second later.
        let now = Instant::now();
        let due: Vec<ToplevelId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.retry_at.is_some_and(|at| at <= now))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            if let Some(win) = self.windows.get_mut(&id) {
                win.retry_at = None;
            }
            self.make(id);
        }
        // Frames that could not be drawn, asked for again.
        for (&id, win) in &mut self.windows {
            if win.redraw_at.is_some_and(|at| at <= now) {
                win.redraw_at = None;
                self.work.frame(FrameEvent::Ready { id });
            }
        }
        if let Some(clipboard) = self.clipboard.as_mut() {
            clipboard.sync(&mut self.client, state, backend);
        }
    }

    fn flush(&mut self) {
        if let Err(error) = self.client.flush() {
            log::warn!("wayland: flush: {error}");
        }
    }

    fn unsent(&self) -> bool {
        self.client.unsent()
    }

    fn screen(&self) -> Option<HostScreen> {
        let output = self.client.outputs().next()?;
        let (width, height) = output.logical_size();
        Some(HostScreen {
            width: u16::try_from(width).ok().filter(|&w| w > 0)?,
            height: u16::try_from(height).ok().filter(|&h| h > 0)?,
            refresh_mhz: u64::try_from(output.refresh_mhz).unwrap_or(0),
        })
    }

    fn next_wakeup(&self) -> Option<Instant> {
        let now = Instant::now();
        let pending = (!self.work.is_empty()).then_some(now);
        let clipboard = self
            .clipboard
            .as_ref()
            .and_then(|clipboard| clipboard.deadline(now));
        let retry = self
            .windows
            .values()
            .flat_map(|w| [w.retry_at, w.redraw_at])
            .flatten()
            .min();
        [pending, clipboard, retry].into_iter().flatten().min()
    }

    fn keymap(&self) -> Option<HostKeymap> {
        self.client.keymap()
    }

    fn output_name(&self) -> &'static str {
        "WAYLAND-1"
    }
}

impl WaylandHost {
    /// Make the windows that have none and wait for no retry: popups with
    /// no shown window to hang from when they were mapped, or whose parent
    /// went.
    fn make_waiting(&mut self) {
        let waiting: Vec<ToplevelId> = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| w.surface.is_none() && w.retry_at.is_none())
            })
            .collect();
        for id in waiting {
            self.make(id);
        }
    }

    /// Hand what the seat queued to the core's queue.
    fn deliver_seat(&mut self) {
        let windows = if self.seat.has_queued() {
            self.windows_by_surface()
        } else {
            HashMap::new()
        };
        self.seat.deliver(&self.client, &mut self.work, &windows);
    }

    /// Each made window's surface, with its id and how much of its backing's
    /// edge its native window leaves out, for the seat.
    fn windows_by_surface(&self) -> HashMap<SurfaceId, (ToplevelId, u16)> {
        self.windows
            .iter()
            .filter_map(|(&id, win)| Some((win.surface?, (id, crop(&win.spec)))))
            .collect()
    }
}

/// How much of the backing's edge a window's native window leaves out: a
/// top-level's X border, which the compositor's frame stands in for;
/// nothing of a popup's.
fn crop(spec: &Spec) -> u16 {
    match spec.kind {
        ToplevelKind::Managed => spec.geometry.border,
        ToplevelKind::Popup { .. } => 0,
    }
}

/// The native window's size: the backing less [`crop`] on each side.
fn shown_size(spec: &Spec) -> (u16, u16) {
    let (width, height) = outer_size(&spec.geometry);
    let border = crop(spec).saturating_mul(2);
    (width.saturating_sub(border), height.saturating_sub(border))
}

/// Where the native window's (0, 0) is in root coordinates.
fn shown_origin(spec: &Spec) -> (i32, i32) {
    let border = i32::from(crop(spec));
    (spec.geometry.x + border, spec.geometry.y + border)
}

/// Copy the `shown` part of a backing `width` pixels wide, `border` in from
/// each edge, into `out`, rows packed.
fn copy_cropped(
    pixels: &[u8],
    width: usize,
    border: usize,
    (shown_w, shown_h): (usize, usize),
    out: &mut [u8],
) {
    if border == 0 {
        let len = out.len().min(pixels.len());
        out[..len].copy_from_slice(&pixels[..len]);
        return;
    }
    let (row, stride) = (shown_w * 4, width * 4);
    for (y, to) in out.chunks_exact_mut(row).take(shown_h).enumerate() {
        let from = (y + border) * stride + border * 4;
        if let Some(from) = pixels.get(from..from + row) {
            to.copy_from_slice(from);
        }
    }
}

/// Backing damage as native-window damage: moved by the crop and clipped to
/// the window, as `(x, y, width, height)`.
fn cropped_damage(
    damage: &[yserver_core::rootless::Rect],
    border: usize,
    (shown_w, shown_h): (usize, usize),
) -> Vec<(u32, u32, u32, u32)> {
    let border = i64::try_from(border).unwrap_or(0);
    let (right, bottom) = (
        i64::try_from(shown_w).unwrap_or(0),
        i64::try_from(shown_h).unwrap_or(0),
    );
    damage
        .iter()
        .filter_map(|r| {
            let x0 = (i64::from(r.x) - border).clamp(0, right);
            let y0 = (i64::from(r.y) - border).clamp(0, bottom);
            let x1 = (i64::from(r.x) + i64::from(r.width) - border).clamp(0, right);
            let y1 = (i64::from(r.y) + i64::from(r.height) - border).clamp(0, bottom);
            (x1 > x0 && y1 > y0).then(|| {
                let to = |v: i64| u32::try_from(v).unwrap_or(0);
                (to(x0), to(y0), to(x1 - x0), to(y1 - y0))
            })
        })
        .collect()
}

/// A window's native content size: its X size and its border (spec §4.0).
fn outer_size(geometry: &Geometry) -> (u16, u16) {
    let border = geometry.border.saturating_mul(2);
    (
        geometry.width.saturating_add(border),
        geometry.height.saturating_add(border),
    )
}

fn size_u32((width, height): (u16, u16)) -> (u32, u32) {
    (u32::from(width), u32::from(height))
}

/// The most bytes of a string the host sends the compositor. A Wayland
/// message holds 4096 bytes, its header and the string's length and
/// padding among them, and a compositor ends the connection of a client
/// that sends a longer one. X properties are as long as a client likes.
const TEXT_LIMIT: usize = 4000;

/// `text` as a Wayland string: up to a NUL, which no Wayland string can
/// hold (a title with one was refused by the compositor, `set_title:
/// BadArgument`), and no longer than [`TEXT_LIMIT`], cut between
/// characters.
fn wire_text(text: &str) -> String {
    let text = text.split('\0').next().unwrap_or_default();
    let mut end = text.len().min(TEXT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// The title, as a Wayland string.
fn title(info: &WindowInfo) -> String {
    wire_text(&info.title)
}

/// `WM_CLASS`'s class, or its instance where a program set only that: what
/// a window rule matches.
fn app_id(info: &WindowInfo) -> String {
    info.class
        .as_ref()
        .map(|(instance, class)| if class.is_empty() { instance } else { class })
        .map(|name| wire_text(name))
        .unwrap_or_default()
}

/// The least and greatest size from `WM_NORMAL_HINTS`, 0 for no limit, as
/// `xdg_toplevel.set_min_size` and `set_max_size` take them. A window whose
/// least size is its greatest is one of a fixed size, which the compositor
/// floats.
fn size_limits(info: &WindowInfo) -> ((u32, u32), (u32, u32)) {
    let hints = &info.size_hints;
    clamped_limits(hints.min.unwrap_or((0, 0)), hints.max.unwrap_or((0, 0)))
}

/// Size limits xdg-shell takes: each at most `i32::MAX`, and a greatest
/// size (0 is none) below the least raised to it, which the compositor
/// would take as a protocol error (`invalid_size`) and end the connection
/// for. X takes any `WM_NORMAL_HINTS`.
fn clamped_limits(min: (u32, u32), max: (u32, u32)) -> ((u32, u32), (u32, u32)) {
    let int = |value: u32| value.min(i32::MAX.unsigned_abs());
    let axis = |min: u32, max: u32| {
        let min = int(min);
        let max = int(max);
        (min, if max != 0 && max < min { min } else { max })
    };
    let (min_w, max_w) = axis(min.0, max.0);
    let (min_h, max_h) = axis(min.1, max.1);
    ((min_w, min_h), (max_w, max_h))
}

/// Whether making `parent` the parent of `id` closes a cycle: `id` itself,
/// or `id` among `parent`'s parents (`parent_of`).
fn makes_cycle(
    id: ToplevelId,
    parent: ToplevelId,
    parent_of: impl Fn(ToplevelId) -> Option<ToplevelId>,
) -> bool {
    let mut at = Some(parent);
    // Bounded: a cycle the windows already have (never made) ends it too.
    for _ in 0..64 {
        match at {
            None => return false,
            Some(window) if window == id => return true,
            Some(window) => at = parent_of(window),
        }
    }
    true
}

/// The `xdg_toplevel.resize_edge` for an EWMH resize edge: xdg-shell's are
/// bits, top 1, bottom 2, left 4, right 8.
fn xdg_edges(edge: ResizeEdge) -> u32 {
    match edge {
        ResizeEdge::TopLeft => 5,
        ResizeEdge::Top => 1,
        ResizeEdge::TopRight => 9,
        ResizeEdge::Right => 8,
        ResizeEdge::BottomRight => 10,
        ResizeEdge::Bottom => 2,
        ResizeEdge::BottomLeft => 6,
        ResizeEdge::Left => 4,
    }
}

/// The name of the process at the other end of the Unix socket `fd`
/// (`SO_PEERCRED`, then `/proc/<pid>/comm`).
fn peer_name(fd: RawFd) -> Option<String> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).ok()?;
    // SAFETY: `cred` and `len` are valid for writing, `len` its size.
    let got = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if got != 0 || cred.pid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", cred.pid)).ok()?;
    let name = comm.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(title: &str, class: Option<(&str, &str)>) -> WindowInfo {
        WindowInfo {
            title: title.to_owned(),
            class: class.map(|(i, c)| (i.to_owned(), c.to_owned())),
            ..WindowInfo::default()
        }
    }

    #[test]
    fn a_title_ends_at_a_nul_which_no_wayland_string_can_hold() {
        assert_eq!(title(&info("Steam", None)), "Steam");
        assert_eq!(title(&info("Game\0junk", None)), "Game");
        assert_eq!(
            title(&info("caf\u{e9} \u{7d42}", None)),
            "caf\u{e9} \u{7d42}"
        );
    }

    /// A title or class longer than a Wayland message holds is cut, between
    /// characters: sent whole, the compositor ends the connection, and with
    /// it every window.
    #[test]
    fn a_title_or_class_fits_a_wayland_message() {
        let long = "\u{fc}".repeat(3000);
        let cut = title(&info(&long, None));
        assert_eq!(cut.len(), TEXT_LIMIT);
        assert!(long.starts_with(&cut));
        // An odd limit falls inside a two-byte character.
        let odd = format!("a{long}");
        assert_eq!(title(&info(&odd, None)).len(), TEXT_LIMIT - 1);
        let class = "c".repeat(70_000);
        assert_eq!(app_id(&info("", Some(("i", &class)))).len(), TEXT_LIMIT);
        assert_eq!(app_id(&info("", Some((&class, "")))).len(), TEXT_LIMIT);
        // The header, the length and the padded string with its NUL.
        const { assert!(8 + 4 + (TEXT_LIMIT + 1).next_multiple_of(4) <= 4096) };
    }

    #[test]
    fn the_app_id_is_the_class_or_the_instance() {
        assert_eq!(
            app_id(&info("", Some(("xmessage", "Xmessage")))),
            "Xmessage"
        );
        assert_eq!(app_id(&info("", Some(("xev", "")))), "xev");
        assert_eq!(app_id(&info("", None)), "");
    }

    /// The root follows a later change of the compositor's screen's size,
    /// not one of its refresh alone, and not a screen it cannot describe.
    #[test]
    fn the_root_follows_the_compositors_screen() {
        let screen = |width, height, refresh_mhz| HostScreen {
            width,
            height,
            refresh_mhz,
        };
        let said = Some(screen(1920, 1080, 60_000));
        assert_eq!(
            screen_layout(said, Some(screen(2560, 1440, 60_000))),
            Some(HostRequest::ScreenLayout {
                outputs: vec![HostOutput {
                    x: 0,
                    y: 0,
                    width: 2560,
                    height: 1440,
                }],
            })
        );
        assert_eq!(screen_layout(said, Some(screen(1920, 1080, 144_000))), None);
        assert_eq!(screen_layout(said, None), None);
        assert!(screen_layout(None, Some(screen(800, 600, 0))).is_some());
    }

    /// A greatest size below the least is raised to it, per axis; none (0)
    /// stays none, and each fits an int.
    #[test]
    fn size_limits_are_ones_xdg_shell_takes() {
        assert_eq!(
            clamped_limits((300, 200), (100, 400)),
            ((300, 200), (300, 400))
        );
        assert_eq!(clamped_limits((300, 200), (0, 0)), ((300, 200), (0, 0)));
        assert_eq!(clamped_limits((0, 0), (50, 60)), ((0, 0), (50, 60)));
        let big = u32::MAX;
        let int = i32::MAX.unsigned_abs();
        assert_eq!(clamped_limits((big, 1), (5, big)), ((int, 1), (int, int)));
    }

    /// A window is not made a dialog of itself or of a window below it:
    /// two windows naming each other, or a longer ring.
    #[test]
    fn a_cycle_of_parents_is_refused() {
        let [a, b, c, d] = [1, 2, 3, 4].map(|n| ToplevelId::fake(0x40_0000 + n));
        // c's parent is b, b's is a.
        let parents = HashMap::from([(c, b), (b, a)]);
        let parent_of = |w: ToplevelId| parents.get(&w).copied();
        assert!(makes_cycle(a, a, parent_of), "itself");
        assert!(makes_cycle(b, c, parent_of), "each other");
        assert!(makes_cycle(a, c, parent_of), "a ring of three");
        assert!(!makes_cycle(d, c, parent_of));
        assert!(!makes_cycle(c, a, parent_of), "already its grandparent");
    }

    #[test]
    fn each_resize_edge_is_an_xdg_edge() {
        let edges: Vec<u32> = [
            ResizeEdge::TopLeft,
            ResizeEdge::Top,
            ResizeEdge::TopRight,
            ResizeEdge::Right,
            ResizeEdge::BottomRight,
            ResizeEdge::Bottom,
            ResizeEdge::BottomLeft,
            ResizeEdge::Left,
        ]
        .into_iter()
        .map(xdg_edges)
        .collect();
        assert_eq!(edges, [5, 1, 9, 8, 10, 2, 6, 4]);
    }

    #[test]
    fn a_toplevel_is_shown_without_its_border_and_a_popup_with_it() {
        let geometry = Geometry {
            x: 10,
            y: 20,
            width: 4,
            height: 2,
            border: 1,
        };
        let spec = |kind| Spec {
            kind,
            geometry,
            info: WindowInfo::default(),
        };
        let top = spec(ToplevelKind::Managed);
        let popup = spec(ToplevelKind::Popup { parent: None });
        assert_eq!((shown_size(&top), shown_origin(&top)), ((4, 2), (11, 21)));
        assert_eq!(
            (shown_size(&popup), shown_origin(&popup)),
            ((6, 4), (10, 20))
        );
        // A 6x4 backing whose pixel (x, y) is the byte x + 10y.
        let pixels: Vec<u8> = (0..4u8)
            .flat_map(|y| (0..6u8).flat_map(move |x| [x + 10 * y; 4]))
            .collect();
        let mut out = vec![0; 4 * 2 * 4];
        copy_cropped(&pixels, 6, 1, (4, 2), &mut out);
        assert_eq!(out[0], 11, "the content's first pixel");
        assert_eq!(out[4 * 4], 21, "its second row");
        let damage = [yserver_core::rootless::Rect {
            x: 0,
            y: 0,
            width: 3,
            height: 6,
        }];
        assert_eq!(cropped_damage(&damage, 1, (4, 2)), [(0, 0, 2, 2)]);
    }

    #[test]
    fn the_native_size_holds_the_border() {
        let geometry = Geometry {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
            border: 2,
        };
        assert_eq!(outer_size(&geometry), (104, 54));
    }
}
