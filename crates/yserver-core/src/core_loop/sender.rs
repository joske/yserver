//! `CoreSender` / `CoreReceiver`: an `mpsc`-style channel that wakes
//! the core's mio poller on every send.
//!
//! Producers (reader threads, libinput, signalfd watcher, setup
//! threads) hold a `CoreSender`; the core owns the `CoreReceiver` and
//! the `Poll`. `NOTIFY_TOKEN` is the token the receiver registers for
//! channel readiness — when a poll iteration sees it, drain the
//! receiver via `try_recv_all`.
//!
//! Every message is also tagged with a generation (see
//! `super::generation`), and *which* generation depends on the producer:
//!
//! - [`CoreSender`] is the process-lifetime handle. It reads the shared
//!   counter at send time. Only process-lifetime messages may travel
//!   this way — they are dispatched whatever their tag, so the value is
//!   never load-bearing.
//! - [`BoundSender`] carries a *fixed* generation, captured when the
//!   producer was created, and stamps every message with it. Every
//!   session-scoped producer — the per-connection setup thread and the
//!   client reader thread it hands off to — must use one.
//!
//! The distinction is the whole quarantine. A session-scoped producer
//! that read the shared counter at send time would tag a message
//! belonging to the OLD session with the NEW generation whenever it woke
//! up after `reset_generation` bumped the counter, and the dispatcher
//! would accept it: an old client's `ClientSetupComplete` inserted into
//! the fresh session, an old reader's `Request` executed against it.
//! Binding at creation makes the tag a property of the producer, which
//! is what "belongs to that session" actually means.
//!
//! `try_recv_all` keeps returning bare `Message`s — its existing callers
//! (across both crates) are unaffected — while `try_recv_all_tagged`
//! additionally hands back each message's tag, for `run_core`'s dispatch
//! loop to apply `generation::should_dispatch` against.

use std::{io, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use mio::{Poll, Token, Waker};

use super::{
    generation::{self, Generation, GenerationCounter},
    message::Message,
};

pub const NOTIFY_TOKEN: Token = Token(0);

#[derive(Clone)]
pub struct CoreSender {
    waker: Arc<Waker>,
    tx: Sender<(Generation, Message)>,
    generation: GenerationCounter,
}

/// A producer handle pinned to one generation.
///
/// Created from a [`CoreSender`] at the moment the producer itself is
/// created — for a client, at accept — and tags every message with the
/// generation that was running *then*, not the one running when the
/// message is finally sent. Cheap to clone; a clone keeps the same
/// binding, which is how a setup thread hands its generation to the
/// reader thread it spawns.
#[derive(Clone)]
pub struct BoundSender {
    waker: Arc<Waker>,
    tx: Sender<(Generation, Message)>,
    generation: Generation,
}

pub struct CoreReceiver {
    rx: Receiver<(Generation, Message)>,
    generation: GenerationCounter,
}

/// Build the (poll, sender, receiver) triple. The waker is registered
/// against `NOTIFY_TOKEN`; producers calling `CoreSender::send` will
/// cause the next `poll.poll()` to surface that token. Sender and
/// receiver share one `GenerationCounter`, created fresh at generation 0.
pub fn channel() -> io::Result<(Poll, CoreSender, CoreReceiver)> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), NOTIFY_TOKEN)?);
    let (tx, rx) = crossbeam_channel::unbounded();
    let generation = GenerationCounter::new();
    Ok((
        poll,
        CoreSender {
            waker,
            tx,
            generation: generation.clone(),
        },
        CoreReceiver { rx, generation },
    ))
}

/// The one place a `(Generation, Message)` reaches the channel, shared
/// by both handles so the tag is the only thing that differs between
/// them.
fn post(
    waker: &Waker,
    tx: &Sender<(Generation, Message)>,
    generation: Generation,
    m: Message,
) -> io::Result<()> {
    tx.send((generation, m))
        .map_err(|_| io::Error::other("core receiver dropped"))?;
    waker.wake()
}

impl CoreSender {
    /// Send a **process-lifetime** message. The tag is read from the
    /// shared counter, which is sound only because such messages
    /// dispatch regardless of their tag (`generation::is_session_scoped`
    /// classifies them; `should_dispatch` waives the match). A
    /// session-scoped message must go through a [`BoundSender`] instead
    /// — sending one here would tag it with whatever generation happens
    /// to be running at send time, which is exactly the cross-session
    /// leak the quarantine exists to stop.
    pub fn send(&self, m: Message) -> io::Result<()> {
        debug_assert!(
            !generation::is_session_scoped(&m),
            "session-scoped message sent through an unbound CoreSender: {m:?} — \
             session-scoped producers must hold a BoundSender (CoreSender::bind)"
        );
        post(&self.waker, &self.tx, self.generation.current(), m)
    }

    /// Cheap clone for handing to producer threads.
    #[must_use]
    pub fn clone_handle(&self) -> Self {
        self.clone()
    }

    /// Bind a new producer to the generation running *now*. The call
    /// site must be where the producer comes into existence — for a
    /// client, the accept — not where it eventually sends.
    #[must_use]
    pub fn bind(&self) -> BoundSender {
        self.bind_to(self.generation.current())
    }

    /// Bind to a generation established elsewhere. Used once, to give a
    /// reader thread the generation its setup thread was bound to, which
    /// travels with `Message::ClientSetupComplete`.
    #[must_use]
    pub fn bind_to(&self, generation: Generation) -> BoundSender {
        BoundSender {
            waker: self.waker.clone(),
            tx: self.tx.clone(),
            generation,
        }
    }
}

impl BoundSender {
    /// Send tagged with this producer's fixed generation.
    pub fn send(&self, m: Message) -> io::Result<()> {
        post(&self.waker, &self.tx, self.generation, m)
    }

    /// The generation this producer belongs to.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Cheap clone for handing to producer threads. Keeps the binding.
    #[must_use]
    pub fn clone_handle(&self) -> Self {
        self.clone()
    }
}

impl CoreReceiver {
    /// Drain everything currently buffered, discarding each message's
    /// generation tag. Non-blocking; stops at the first empty
    /// `try_recv`. This is the pre-existing, behaviour-preserving
    /// accessor — every caller that isn't `run_core`'s dispatch loop
    /// uses this one and is unaffected by generation tagging.
    pub fn try_recv_all(&self) -> impl Iterator<Item = Message> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok().map(|(_, m)| m))
    }

    /// Same drain, keeping each message's generation tag. Used by
    /// `run_core` to apply `generation::should_dispatch` at the top of
    /// dispatch.
    pub fn try_recv_all_tagged(&self) -> impl Iterator<Item = (Generation, Message)> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }

    /// The generation the core loop is currently running.
    #[must_use]
    pub fn current_generation(&self) -> Generation {
        self.generation.current()
    }

    /// A clone of the shared counter. The core loop is the sole owner of
    /// `CoreReceiver`, so this is also the sole handle from which a
    /// future reset (steps 4/5) may call `GenerationCounter::bump`.
    #[must_use]
    pub fn generation_counter(&self) -> GenerationCounter {
        self.generation.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sender_wakes_poll() {
        let (mut poll, sender, _rx) = channel().unwrap();
        sender.clone_handle().send(Message::Shutdown).unwrap();
        let mut events = mio::Events::with_capacity(4);
        poll.poll(&mut events, Some(Duration::from_millis(50)))
            .unwrap();
        assert!(events.iter().any(|e| e.token() == NOTIFY_TOKEN));
    }

    #[test]
    fn untagged_try_recv_all_is_unaffected_by_tagging() {
        let (_poll, sender, rx) = channel().unwrap();
        sender.send(Message::Shutdown).unwrap();
        assert!(matches!(rx.try_recv_all().next(), Some(Message::Shutdown)));
    }

    #[test]
    fn a_bound_sender_keeps_tagging_with_the_generation_it_captured() {
        // The quarantine's core property: the tag follows the PRODUCER,
        // not the clock. A producer bound before a reset stays stale
        // however many generations go by before it sends.
        let (_poll, sender, rx) = channel().unwrap();
        let old = rx.current_generation();
        let bound = sender.bind();
        let bumped = rx.generation_counter().bump();
        assert_ne!(old, bumped);

        bound.send(Message::Shutdown).unwrap();
        bound.clone_handle().send(Message::Shutdown).unwrap();
        rx.generation_counter().bump();
        bound.send(Message::Shutdown).unwrap();

        let tagged: Vec<_> = rx.try_recv_all_tagged().collect();
        assert_eq!(tagged.len(), 3);
        for (tag, _) in &tagged {
            assert_eq!(*tag, old, "a bound producer never re-reads the counter");
        }
        assert_eq!(bound.generation(), old);
    }

    #[test]
    fn a_sender_bound_after_a_bump_is_current() {
        // The other half: binding at accept must not make a connection
        // accepted in the new generation stale.
        let (_poll, sender, rx) = channel().unwrap();
        let bumped = rx.generation_counter().bump();
        let bound = sender.bind();
        bound.send(Message::Shutdown).unwrap();
        let tagged: Vec<_> = rx.try_recv_all_tagged().collect();
        assert_eq!(tagged[0].0, bumped);
        assert_eq!(tagged[0].0, rx.current_generation());
    }

    #[test]
    fn bind_to_hands_one_producers_generation_to_another() {
        // How a setup thread's generation reaches the reader thread the
        // core spawns for that client.
        let (_poll, sender, rx) = channel().unwrap();
        let setup = sender.bind();
        rx.generation_counter().bump();
        let reader = sender.bind_to(setup.generation());
        reader.send(Message::Shutdown).unwrap();
        let tagged: Vec<_> = rx.try_recv_all_tagged().collect();
        assert_eq!(tagged[0].0, setup.generation());
        assert_ne!(tagged[0].0, rx.current_generation());
    }

    #[test]
    fn messages_are_tagged_with_the_generation_in_effect_at_send_time() {
        let (_poll, sender, rx) = channel().unwrap();
        let initial = rx.current_generation();
        sender.send(Message::Shutdown).unwrap();
        let bumped = rx.generation_counter().bump();
        sender.send(Message::Shutdown).unwrap();

        let tagged: Vec<_> = rx.try_recv_all_tagged().collect();
        assert_eq!(tagged.len(), 2);
        assert_eq!(tagged[0].0, initial);
        assert_eq!(tagged[1].0, bumped);
        assert_ne!(initial, bumped);
    }
}
