//! Peer identity of a client connection.
//!
//! Capture only: no policy lives here. See
//! docs/superpowers/specs/2026-10-30-xhost-access-control-design.md, "Peer
//! identity, captured once".

/// Credentials of a Unix-socket peer, as the kernel reports them at
/// `connect()` time: effective uid and primary gid, never supplementary
/// groups (Xorg's `GetLocalClientCreds` on its `SO_PEERCRED` and
/// `getpeereid` paths, `os/access.c:1246-1274`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    /// `None` where the platform cannot report it (FreeBSD).
    pub pid: Option<u32>,
}
