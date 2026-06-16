//! Token assignment for the core's mio poller, plus monotonic
//! `ClientId` allocation.
//!
//! The poller's tokens fall into two ranges:
//!
//! - Fixed system tokens at the bottom: notify-channel, listener,
//!   DRM, signalfd, libinput, host-X11. These never change at
//!   runtime.
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
/// DRM device fd; readiness drives `Backend::on_page_flip_ready`.
pub const DRM_TOKEN: Token = Token(2);
/// signalfd; readiness causes the core to issue `Message::Shutdown`.
pub const SIGNAL_TOKEN: Token = Token(3);
/// libinput epoll fd; readiness drives the libinput thread (KMS) or
/// the core directly (per phase).
pub const LIBINPUT_TOKEN: Token = Token(4);
/// Host X11 connection fd; F2 reroutes host I/O onto the core poller.
pub const HOST_X11_TOKEN: Token = Token(5);
/// Stage 5 Task 6.1: backend-internal epoll FD aggregating per-entry
/// sync_file FDs for deferred PRESENT completion. Readiness drives
/// `Backend::drain_completed_present_events`.
pub const PRESENT_COMPLETION_TOKEN: Token = Token(6);
/// libseat connection fd; readiness drives `Backend::on_seat_ready`.
pub const SEAT_TOKEN: Token = Token(7);
/// DRM-subsystem udev hotplug monitor fd; readiness drives
/// `Backend::on_display_hotplug`.
pub const DRM_HOTPLUG_TOKEN: Token = Token(8);

/// First token usable for per-client writers. Picked far above the
/// fixed system tokens so they're cheap to recognise on a hot poll.
const CLIENT_TOKEN_BASE: usize = 0x1000;

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
        for tok in [
            NOTIFY_TOKEN,
            LISTENER_TOKEN,
            DRM_TOKEN,
            SIGNAL_TOKEN,
            LIBINPUT_TOKEN,
            HOST_X11_TOKEN,
            PRESENT_COMPLETION_TOKEN,
            SEAT_TOKEN,
            DRM_HOTPLUG_TOKEN,
        ] {
            assert!(token_to_client(tok).is_none(), "{tok:?}");
        }
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
        let all = [
            NOTIFY_TOKEN.0,
            LISTENER_TOKEN.0,
            DRM_TOKEN.0,
            SIGNAL_TOKEN.0,
            LIBINPUT_TOKEN.0,
            HOST_X11_TOKEN.0,
            PRESENT_COMPLETION_TOKEN.0,
            SEAT_TOKEN.0,
            DRM_HOTPLUG_TOKEN.0,
        ];
        let mut sorted: Vec<_> = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }
}
