//! `CoreSender` / `CoreReceiver`: an `mpsc`-style channel that wakes
//! the core's mio poller on every send.
//!
//! Producers (reader threads, libinput, signalfd watcher, setup
//! threads) hold a `CoreSender`; the core owns the `CoreReceiver` and
//! the `Poll`. `NOTIFY_TOKEN` is the token the receiver registers for
//! channel readiness — when a poll iteration sees it, drain the
//! receiver via `try_recv_all`.
//!
//! Every message is also tagged, at send time, with the generation the
//! loop was running when it was produced (see `super::generation`).
//! `try_recv_all` keeps returning bare `Message`s — its existing callers
//! (across both crates) are unaffected — while `try_recv_all_tagged`
//! additionally hands back each message's tag, for `run_core`'s dispatch
//! loop to apply `generation::should_dispatch` against.

use std::{io, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use mio::{Poll, Token, Waker};

use super::{
    generation::{Generation, GenerationCounter},
    message::Message,
};

pub const NOTIFY_TOKEN: Token = Token(0);

#[derive(Clone)]
pub struct CoreSender {
    waker: Arc<Waker>,
    tx: Sender<(Generation, Message)>,
    generation: GenerationCounter,
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

impl CoreSender {
    pub fn send(&self, m: Message) -> io::Result<()> {
        self.tx
            .send((self.generation.current(), m))
            .map_err(|_| io::Error::other("core receiver dropped"))?;
        self.waker.wake()
    }

    /// Cheap clone for handing to producer threads.
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
