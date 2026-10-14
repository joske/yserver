//! The channel from a window shell (and its display clock) into the core
//! loop: events queue here, and one `Message::PlatformEvent` wakes the core
//! until it drains them. Input goes straight to the core as
//! `Message::HostInput`, the KMS input thread's channel.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use yserver_core::core_loop::{CoreSender, HostInputEvent, Message};

use super::input::devices;

/// What the shell reports about a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellEvent {
    /// The window became visible (`true`) or fully occluded or minimised.
    Visible(bool),
    /// The window became key; `caps` is the window system's Caps Lock.
    LockState { caps: bool },
    /// The window's content is now `width`x`height` X pixels, `scale`
    /// backing pixels per X pixel, `mm` millimetres (0: unknown): a user
    /// resize or scale change (`request` none), or the completion of the
    /// core's `ShellUi::set_content_size` with that token.
    #[cfg_attr(all(kms, not(test)), allow(dead_code))] // Posted by a window shell.
    Resized {
        width: u16,
        height: u16,
        scale: u32,
        mm: (u32, u32),
        request: Option<u64>,
    },
}

/// The shell's side of the core channel; shared by every shell thread.
#[derive(Default)]
pub(crate) struct ShellLink {
    sender: Mutex<Option<CoreSender>>,
    events: Mutex<Vec<ShellEvent>>,
    /// A `PlatformEvent` is in flight (or owed once connected).
    posted: AtomicBool,
    /// Shutdown was asked for before the core connected.
    shutdown_requested: AtomicBool,
}

impl ShellLink {
    /// Attach the core loop's channel; announces the input devices, then
    /// delivers what was posted before.
    pub(crate) fn connect(&self, sender: CoreSender) {
        let mut slot = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for event in devices::announcements(crate::clock::server_time_ms()) {
            let _ = sender.send(Message::HostInput(event));
        }
        if self.shutdown_requested.load(Ordering::Acquire) {
            let _ = sender.send(Message::Shutdown);
        }
        if self.posted.load(Ordering::Acquire) {
            let _ = sender.send(Message::PlatformEvent);
        }
        *slot = Some(sender);
    }

    /// Detach the core loop's channel once it stopped.
    pub(crate) fn disconnect(&self) {
        *self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Wake the core loop once until it drains.
    pub(crate) fn wake(&self) {
        if self.posted.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = sender.send(Message::PlatformEvent);
        }
    }

    /// Queue `event` and wake the core loop.
    pub(crate) fn post(&self, event: ShellEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        self.wake();
    }

    /// Send the input `build` makes at the current server time; nothing,
    /// and `build` is not called, before the core connected.
    pub(crate) fn send_input(&self, build: impl FnOnce(u32, &mut Vec<HostInputEvent>)) {
        let slot = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sender) = slot.as_ref() else {
            return;
        };
        let mut events = Vec::new();
        build(crate::clock::server_time_ms(), &mut events);
        for event in events {
            let _ = sender.send(Message::HostInput(event));
        }
    }

    /// Ask the core loop to shut down (window closed, Quit).
    pub(crate) fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = sender.send(Message::Shutdown);
        }
    }

    /// The core loop's drain: re-arm the wake, then take the events.
    pub(crate) fn drain(&self) -> Vec<ShellEvent> {
        self.posted.store(false, Ordering::Release);
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

#[cfg(test)]
mod tests {
    use yserver_core::core_loop::{self, HostInputEvent, Message};

    use super::{ShellEvent, ShellLink};
    use crate::surface::input::translate::key_event;

    fn received(rx: &core_loop::CoreReceiver) -> Vec<Message> {
        rx.try_recv_all().filter(|m| !is_announcement(m)).collect()
    }

    fn is_announcement(message: &Message) -> bool {
        matches!(
            message,
            Message::HostInput(HostInputEvent::DeviceAdded(_) | HostInputEvent::Key(_))
        )
    }

    #[test]
    fn devices_are_announced_before_any_input_and_nothing_precedes_connect() {
        let link = ShellLink::default();
        let mut called = false;
        link.send_input(|_, _| called = true);
        assert!(!called, "no input is built before the core connects");
        let (_poll, sender, rx) = core_loop::channel().expect("channel");
        link.connect(sender);
        link.send_input(|time, out| out.push(key_event(38, true, time)));
        let messages: Vec<Message> = rx.try_recv_all().collect();
        let kinds: Vec<&str> = messages
            .iter()
            .map(|m| match m {
                Message::HostInput(HostInputEvent::DeviceAdded(_)) => "added",
                Message::HostInput(HostInputEvent::Key(key)) if key.keycode == 38 => "a",
                Message::HostInput(HostInputEvent::Key(_)) => "num-lock",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["added", "added", "num-lock", "num-lock", "a"]);
    }

    #[test]
    fn a_disconnected_link_drops_input() {
        let link = ShellLink::default();
        let (_poll, sender, rx) = core_loop::channel().expect("channel");
        link.connect(sender);
        let _ = rx.try_recv_all().count();
        link.disconnect();
        link.send_input(|time, out| out.push(key_event(38, true, time)));
        assert_eq!(rx.try_recv_all().count(), 0);
    }

    #[test]
    fn wakes_coalesce_until_drained_and_survive_an_early_post() {
        let link = ShellLink::default();
        link.post(ShellEvent::Visible(false));
        let (_poll, sender, rx) = core_loop::channel().expect("channel");
        link.connect(sender);
        link.wake();
        link.post(ShellEvent::Visible(true));
        let messages = received(&rx);
        assert_eq!(messages.len(), 1, "one wake for three posts: {messages:?}");
        assert!(matches!(messages[0], Message::PlatformEvent));
        assert_eq!(
            link.drain(),
            [ShellEvent::Visible(false), ShellEvent::Visible(true)]
        );
        link.wake();
        assert_eq!(received(&rx).len(), 1, "a drain re-arms the wake");
    }

    #[test]
    fn shutdown_before_connect_is_delivered_on_connect() {
        let link = ShellLink::default();
        link.request_shutdown();
        let (_poll, sender, rx) = core_loop::channel().expect("channel");
        link.connect(sender);
        let messages = received(&rx);
        assert!(matches!(messages.as_slice(), [Message::Shutdown]));
        link.disconnect();
        link.request_shutdown();
        assert!(received(&rx).is_empty(), "disconnected");
    }
}
