//! Token assignment for the core's mio poller, plus monotonic
//! `ClientId` allocation.
//!
//! The poller's tokens fall into three ranges:
//!
//! - Fixed system tokens at the bottom: notify-channel, listener, and
//!   signalfd. These never change at runtime.
//! - Backend-owned fds, allocated densely from `0x100` in the order returned
//!   by `Backend::poll_fds`. Every fd gets its own token so readiness can be
//!   routed back to that exact source, even when a backend exposes multiple
//!   fds of the same kind (for example one DRM fd per KMS device).
//! - Per-client writer tokens, allocated densely from `0x1000`
//!   upwards using each client's `ClientId`. The mapping is bijective
//!   so the poller can decode a `WRITABLE`-readiness token straight
//!   back to a `ClientId`.
//!
//! `ClientId` allocation is monotonic by design — see codex's missing
//! test bullet on stale-token reuse. A disconnected client never
//! returns to the same id, so a leftover `WRITABLE` event for a torn
//! down client cannot accidentally reach a freshly-connected client
//! that recycled the id.
//!
//! `NOTIFY_TOKEN` is re-exported here so callers can pull every poll
//! token from one place; its source-of-truth lives next to the Waker
//! that registers it (`core_loop::sender::NOTIFY_TOKEN`).

use std::sync::atomic::{AtomicU32, Ordering};

use mio::Token;
use yserver_protocol::x11::ClientId;

pub use super::sender::NOTIFY_TOKEN;

/// `UnixListener` accepting connections from clients.
pub const LISTENER_TOKEN: Token = Token(1);
/// signalfd; readiness causes the core to issue `Message::Shutdown`.
pub const SIGNAL_TOKEN: Token = Token(3);

/// First token usable for backend-owned poll sources. The core keeps the
/// corresponding `(fd, BackendFdKind)` records in the same order.
const BACKEND_TOKEN_BASE: usize = 0x100;

/// First token usable for per-client writers. Picked far above the
/// backend range so both classes are cheap to recognise on a hot poll.
const CLIENT_TOKEN_BASE: usize = 0x1000;

/// Map an index in the core's backend poll-source table to a unique token.
/// Returns `None` when the table would overlap client tokens.
#[must_use]
pub fn backend_token(index: usize) -> Option<Token> {
    let raw = BACKEND_TOKEN_BASE.checked_add(index)?;
    (raw < CLIENT_TOKEN_BASE).then_some(Token(raw))
}

/// Inverse of [`backend_token`]. Tokens outside the backend-owned range
/// return `None`.
#[must_use]
pub fn token_to_backend_index(token: Token) -> Option<usize> {
    if (BACKEND_TOKEN_BASE..CLIENT_TOKEN_BASE).contains(&token.0) {
        Some(token.0 - BACKEND_TOKEN_BASE)
    } else {
        None
    }
}

/// Map a `ClientId` to the token used for its writer fd in the
/// poller.
#[must_use]
pub fn client_token(id: ClientId) -> Token {
    Token(CLIENT_TOKEN_BASE + id.0 as usize)
}

/// Inverse of [`client_token`]: decode a poll token back into the
/// `ClientId` it represents, or `None` if the token is one of the
/// fixed system tokens (or otherwise out of range).
#[must_use]
pub fn token_to_client(t: Token) -> Option<ClientId> {
    let raw = t.0;
    if raw < CLIENT_TOKEN_BASE {
        return None;
    }
    let offset = raw - CLIENT_TOKEN_BASE;
    let id = u32::try_from(offset).ok()?;
    Some(ClientId(id))
}

/// Monotonic `ClientId` allocator. Starts at 1 so id 0 stays
/// reserved for the server itself (`SERVER_OWNER` in `resources.rs`).
#[derive(Debug)]
pub struct ClientIdAllocator {
    next: AtomicU32,
}

impl Default for ClientIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientIdAllocator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: AtomicU32::new(1),
        }
    }

    /// Hand out the next id. Wraps at `u32::MAX` (the server runs
    /// long enough that this is essentially impossible — wrap is
    /// defensive against pathological mis-use).
    pub fn allocate(&self) -> ClientId {
        ClientId(self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// Peek at the next id without advancing.
    #[must_use]
    pub fn peek(&self) -> ClientId {
        ClientId(self.next.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_token_round_trips() {
        for raw in [1u32, 2, 100, 0x1000, 0xFFFF_FFFE] {
            let id = ClientId(raw);
            let tok = client_token(id);
            assert_eq!(token_to_client(tok), Some(id));
        }
    }

    #[test]
    fn system_tokens_decode_to_none() {
        for tok in [NOTIFY_TOKEN, LISTENER_TOKEN, SIGNAL_TOKEN] {
            assert!(token_to_client(tok).is_none(), "{tok:?}");
        }
    }

    #[test]
    fn backend_tokens_are_unique_and_round_trip() {
        for index in [0, 1, 2, 100, CLIENT_TOKEN_BASE - BACKEND_TOKEN_BASE - 1] {
            let token = backend_token(index).expect("backend index should fit");
            assert_eq!(token_to_backend_index(token), Some(index));
            assert!(token_to_client(token).is_none());
        }
        assert_ne!(backend_token(0), backend_token(1));
        assert!(backend_token(CLIENT_TOKEN_BASE - BACKEND_TOKEN_BASE).is_none());
        assert_eq!(token_to_backend_index(LISTENER_TOKEN), None);
        assert_eq!(token_to_backend_index(client_token(ClientId(1))), None);
    }

    #[test]
    fn allocator_is_monotonic() {
        let alloc = ClientIdAllocator::new();
        let a = alloc.allocate();
        let b = alloc.allocate();
        let c = alloc.allocate();
        assert_eq!(a, ClientId(1));
        assert_eq!(b, ClientId(2));
        assert_eq!(c, ClientId(3));
        // No "release" — disconnect/reconnect must hand out fresh ids.
        assert_eq!(alloc.peek(), ClientId(4));
    }

    #[test]
    fn fixed_tokens_are_distinct() {
        // Sanity: catches accidental duplicate constants.
        let all = [NOTIFY_TOKEN.0, LISTENER_TOKEN.0, SIGNAL_TOKEN.0];
        let mut sorted: Vec<_> = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }
}
