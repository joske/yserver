//! Process-lifetime generation counter and message-tagging rules.
//!
//! Step 2 of `docs/superpowers/plans/2026-09-09-server-reset-plan.md`:
//! every `Message` sent over the core channel is tagged, at send time
//! (`CoreSender::send`), with the generation the loop was running when
//! the message was produced. The counter lives beside the core loop
//! (reachable through `CoreReceiver`), never inside `ServerState` — see
//! `docs/superpowers/specs/2026-09-09-server-reset-design.md`, "The
//! generation boundary".
//!
//! Nothing bumps the counter in this step, so `should_dispatch` is
//! always true today and behaviour is unchanged: `GenerationCounter::
//! bump` exists only so this step's tests (and the future reset
//! boundary, steps 4/5) have something to call.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use super::message::Message;

/// A generation ordinal. Cheap to copy/compare; carries no behaviour of
/// its own. Numeric identity is deliberately not exposed — nothing
/// outside this module needs to construct one except by reading a
/// `GenerationCounter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Generation(u64);

/// Shared, process-lifetime counter. `CoreSender` holds a clone to tag
/// every message it sends; `CoreReceiver` holds another so the core loop
/// can read the generation it is currently running (and, once the reset
/// boundary lands, bump it). Not part of `ServerState`.
#[derive(Clone, Default)]
pub struct GenerationCounter(Arc<AtomicU64>);

impl GenerationCounter {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// The generation currently in effect.
    #[must_use]
    pub fn current(&self) -> Generation {
        Generation(self.0.load(Ordering::Acquire))
    }

    /// Advance to a new generation, returning it. Not called from any
    /// step 1/2 production path — that is the reset boundary (steps 4/5)
    /// — but needed to construct the "old generation" half of this
    /// step's tests.
    pub fn bump(&self) -> Generation {
        Generation(self.0.fetch_add(1, Ordering::AcqRel) + 1)
    }
}

/// Fixed by the design doc's message-tagging table: whether `message` is
/// session-scoped (discarded when its tag doesn't match the generation
/// currently running) or process-lifetime (always processed, no matter
/// its tag). Exhaustive match — a new `Message` variant must be
/// classified here explicitly rather than silently landing in either
/// bucket.
pub(crate) fn is_session_scoped(message: &Message) -> bool {
    match message {
        Message::SetupAllocate { .. }
        | Message::ClientSetupComplete { .. }
        | Message::Request { .. }
        | Message::ClientDisconnected { .. } => true,

        Message::HostInput(_)
        | Message::CrtcConfigReady
        | Message::Shutdown
        | Message::VtRelease
        | Message::VtAcquire
        | Message::SwitchVt(_)
        | Message::DumpScanout
        | Message::DumpDrawables => false,
    }
}

/// Whether a message tagged with `message_generation`, arriving while
/// the loop is running `current_generation`, should be dispatched.
/// Session-scoped messages must match; process-lifetime messages are
/// unconditional. Applied at the top of `run_core`'s dispatch loop.
pub(crate) fn should_dispatch(
    current_generation: Generation,
    message_generation: Generation,
    message: &Message,
) -> bool {
    message_generation == current_generation || !is_session_scoped(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_loop::message::HostInputEvent;
    use yserver_protocol::x11::ClientId;

    fn device_removed(node: &str) -> Message {
        Message::HostInput(HostInputEvent::DeviceRemoved {
            device_node: node.into(),
        })
    }

    #[test]
    fn counter_starts_at_generation_zero() {
        let counter = GenerationCounter::new();
        assert_eq!(counter.current(), Generation::default());
    }

    #[test]
    fn bump_advances_and_is_visible_to_every_clone() {
        let counter = GenerationCounter::new();
        let clone = counter.clone();
        let next = counter.bump();
        assert_eq!(clone.current(), next);
        assert_ne!(clone.current(), Generation::default());
    }

    #[test]
    fn session_scoped_message_is_dropped_on_generation_mismatch() {
        let old = Generation::default();
        let current = GenerationCounter::new();
        current.bump();
        let msg = Message::ClientDisconnected {
            id: ClientId(1),
            reason: std::io::Error::other("gone"),
        };
        assert!(!should_dispatch(current.current(), old, &msg));
    }

    #[test]
    fn session_scoped_message_is_processed_when_generation_matches() {
        let counter = GenerationCounter::new();
        let msg = Message::ClientDisconnected {
            id: ClientId(1),
            reason: std::io::Error::other("gone"),
        };
        assert!(should_dispatch(counter.current(), counter.current(), &msg));
    }

    #[test]
    fn process_lifetime_message_is_processed_regardless_of_generation() {
        let old = Generation::default();
        let current = GenerationCounter::new();
        current.bump();
        assert!(should_dispatch(current.current(), old, &Message::Shutdown));
    }

    #[test]
    fn device_removed_tagged_with_an_old_generation_is_still_dispatched() {
        // The important one: discarding this would corrupt
        // `InputInventory` permanently, since the device is gone and no
        // second notification is ever coming.
        let old = Generation::default();
        let current = GenerationCounter::new();
        current.bump();
        let msg = device_removed("/dev/input/event3");
        assert!(should_dispatch(current.current(), old, &msg));
    }

    #[test]
    fn with_the_counter_pinned_every_message_dispatches_as_before() {
        // Step 2 must not change behaviour while the counter never
        // increments: every classification, tagged with generation 0
        // against a current generation of 0, dispatches.
        let counter = GenerationCounter::new();
        let messages = [
            Message::Shutdown,
            Message::CrtcConfigReady,
            Message::VtRelease,
            Message::VtAcquire,
            Message::SwitchVt(1),
            Message::DumpScanout,
            Message::DumpDrawables,
            device_removed("/dev/input/event0"),
            Message::ClientDisconnected {
                id: ClientId(1),
                reason: std::io::Error::other("gone"),
            },
        ];
        for msg in &messages {
            assert!(should_dispatch(counter.current(), counter.current(), msg));
        }
    }
}
