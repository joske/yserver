//! A rootless host served on the core thread: a window system the server
//! is a client of over a socket the core loop polls
//! (`BackendFdKind::HostDisplay`), such as a Wayland compositor. It runs on
//! the card-less KMS platform, which keeps DRI3 and Present on a render
//! node.
//!
//! Its events reach the core as they would from a host with a UI thread of
//! its own, only without the hop: [`DisplayHost::dispatch`] collects what
//! the window system said as [`HostWork`] -- host requests, input and frame
//! events, in the order it said them -- and the platform applies it in that
//! order with the state in hand, after the host is no longer borrowed.

use std::{cell::RefCell, io, os::fd::RawFd, rc::Rc, time::Instant};

use ash::vk;
use yserver_core::{
    backend::{ActiveCursorImage, Backend},
    core_loop::HostInputEvent,
    rootless::{HostRequest, RootlessEvent, ToplevelId},
    server::ServerState,
};

use super::host::{CpuFrame, DmabufFrame, FrameEvent, HandoffKind, RootlessHost};
use crate::kms::vk::device::VkContext;

/// One thing the window system said, for the core or the presenter.
#[derive(Debug)]
pub(crate) enum HostItem {
    Request(HostRequest),
    Input(HostInputEvent),
    #[allow(dead_code, reason = "the Wayland host queues its frame events")]
    Frame(FrameEvent),
}

/// What one dispatch of the host's connection asks of the core and the
/// presenter, in the order the window system said it. One queue, not one
/// per kind: a key press read before a focus change must reach the window
/// that had the focus when it was pressed.
#[derive(Debug, Default)]
pub(crate) struct HostWork(Vec<HostItem>);

impl HostWork {
    pub(crate) fn request(&mut self, request: HostRequest) {
        self.0.push(HostItem::Request(request));
    }

    pub(crate) fn input(&mut self, event: HostInputEvent) {
        self.0.push(HostItem::Input(event));
    }

    #[allow(dead_code, reason = "the Wayland host queues its frame events")]
    pub(crate) fn frame(&mut self, event: FrameEvent) {
        self.0.push(HostItem::Frame(event));
    }
}

impl IntoIterator for HostWork {
    type Item = HostItem;
    type IntoIter = std::vec::IntoIter<HostItem>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// The host's screen: the root's size and the refresh of its one RandR
/// output (spec §5: one host output until per-output RandR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostScreen {
    pub width: u16,
    pub height: u16,
    /// Millihertz; 0 when the window system gave none.
    pub refresh_mhz: u64,
}

/// A host whose display connection the core loop polls.
pub(crate) trait DisplayHost: RootlessHost {
    /// The connection the core loop polls.
    fn fd(&self) -> RawFd;
    /// Read what the window system sent, without blocking, into `work`.
    ///
    /// # Errors
    ///
    /// The window system closed the connection.
    fn dispatch(&mut self, work: &mut HostWork) -> io::Result<()>;
    /// Once per loop iteration, with the state in hand, for work that is not
    /// a window's (a clipboard bridge). The host must not call back into the
    /// platform's rootless state from here.
    fn service(&mut self, _state: &mut ServerState, _backend: &mut dyn Backend) {}
    /// Send what is queued, without blocking: it runs on the core thread.
    /// What the socket does not take now stays queued, and the next call
    /// (the platform calls this at least once each loop iteration, before
    /// the loop blocks) sends it.
    fn flush(&mut self) {}
    /// The host's screen.
    fn screen(&self) -> Option<HostScreen>;
    /// When the loop must come round for the host's own work.
    fn next_wakeup(&self) -> Option<Instant> {
        None
    }
    /// The window system's keyboard layout and variant, which the server's
    /// keymap takes at start, so a key means in X what it means in the
    /// host's other windows.
    fn keyboard_layout(&self) -> Option<(String, String)> {
        None
    }
    /// The name of the RandR output the host's screen is.
    fn output_name(&self) -> &'static str {
        "HOST-1"
    }
}

/// A [`DisplayHost`] shared between the platform, which polls it, and the
/// presenter, which hands it frames.
pub(crate) type SharedHost = Rc<RefCell<dyn DisplayHost>>;

/// The presenter's handle on a [`SharedHost`].
pub(crate) struct PresenterHandle(pub SharedHost);

impl RootlessHost for PresenterHandle {
    fn handoff(&self, id: ToplevelId, depth: u8) -> HandoffKind {
        self.0.borrow().handoff(id, depth)
    }

    fn create_surface(&mut self, id: ToplevelId, vk: &VkContext) -> io::Result<vk::SurfaceKHR> {
        self.0.borrow_mut().create_surface(id, vk)
    }

    fn submit_cpu(&mut self, frame: CpuFrame) {
        self.0.borrow_mut().submit_cpu(frame);
    }

    fn submit_dmabuf(&mut self, frame: DmabufFrame) {
        self.0.borrow_mut().submit_dmabuf(frame);
    }

    fn first_frame(&mut self, id: ToplevelId) {
        self.0.borrow_mut().first_frame(id);
    }

    fn on_events(&mut self, events: &[RootlessEvent]) {
        self.0.borrow_mut().on_events(events);
    }

    fn frame_wanted(&mut self, id: ToplevelId) {
        self.0.borrow_mut().frame_wanted(id);
    }

    fn set_cursor(&mut self, cursor: Option<&ActiveCursorImage>) {
        self.0.borrow_mut().set_cursor(cursor);
    }
}
