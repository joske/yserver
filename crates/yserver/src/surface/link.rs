//! The channel from a window shell (and its display clock) into the core
//! loop: events queue here, and one `Message::PlatformEvent` wakes the core
//! until it drains them.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use yserver_core::core_loop::{CoreSender, Message};

/// What the shell reports about a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellEvent {
    /// The window became visible (`true`) or fully occluded or minimised.
    Visible(bool),
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
    /// Attach the core loop's channel; delivers what was posted before.
    pub(crate) fn connect(&self, sender: CoreSender) {
        let mut slot = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    use yserver_core::core_loop::{self, Message};

    use super::{ShellEvent, ShellLink};

    fn received(rx: &core_loop::CoreReceiver) -> Vec<Message> {
        rx.try_recv_all().collect()
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
