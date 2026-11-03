//! Synchronous policy hooks for server access decisions.

use std::ops::BitOr;

use yserver_protocol::x11::ClientId;

use crate::{peer::PeerIdentity, server::ServerState};

/// A policy's answer. `Allow` means this policy does not object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow,
    Deny(DenyError),
}

/// The X error a policy would like the checked site to report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DenyError {
    pub code: u8,
    pub value: u32,
}

/// `BadAccess` with no associated value.
pub const ACCESS: DenyError = DenyError {
    code: yserver_protocol::x11::error::BAD_ACCESS,
    value: 0,
};

/// Server operation categories passed to policy hooks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccessMode(u32);

impl AccessMode {
    pub const READ: Self = Self(1 << 0);
    pub const MANAGE: Self = Self(1 << 1);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for AccessMode {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// The request opcodes active at an access check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestOpcodes {
    pub major: u8,
    pub minor: u8,
}

/// The credential that admitted a client, without retaining its secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Cookie {
        credential: CredentialId,
        source: CredentialSource,
    },
    HostAccess,
    Internal,
}

/// Where an accepted cookie came from.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialSource {
    File,
    Session,
}

/// An opaque process-local identifier for an accepted credential.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CredentialId(u64);

impl CredentialId {
    pub(crate) const fn from_counter(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for CredentialId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Facts available to a synchronous policy callback.
pub struct AccessContext<'a> {
    state: &'a ServerState,
    request: Option<RequestOpcodes>,
}

impl<'a> AccessContext<'a> {
    pub(crate) const fn new(state: &'a ServerState, request: Option<RequestOpcodes>) -> Self {
        Self { state, request }
    }

    #[must_use]
    pub fn peer(&self, client: ClientId) -> Option<&PeerIdentity> {
        self.state.clients.get(&client.0).map(|state| &state.peer)
    }

    #[must_use]
    pub fn admission(&self, client: ClientId) -> Option<&Admission> {
        self.state
            .clients
            .get(&client.0)
            .map(|state| &state.admission)
    }

    #[must_use]
    pub const fn request(&self) -> Option<RequestOpcodes> {
        self.request
    }
}

/// An ordered, synchronous access policy. Methods run inline on the core loop
/// and must do no I/O or blocking wait.
pub trait AccessPolicy: Send + Sync {
    fn server(&self, _ctx: &AccessContext<'_>, _client: ClientId, _mode: AccessMode) -> Decision {
        Decision::Allow
    }

    fn client_connected(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        _peer: &PeerIdentity,
        _admission: &Admission,
    ) {
    }

    fn connection_gone(&self, _ctx: &AccessContext<'_>, _client: ClientId) {}

    fn owner_gone(&self, _client: ClientId) {}

    fn generation_ended(&self) {}
}
