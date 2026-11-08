//! Synchronous policy hooks for server access decisions.

use std::ops::BitOr;

use yserver_protocol::x11::{AtomId, ClientId, RequestHeader, ResourceId};

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
    pub const WRITE: Self = Self(1 << 2);
    pub const DESTROY: Self = Self(1 << 3);
    pub const CREATE: Self = Self(1 << 4);
    pub const GET_ATTR: Self = Self(1 << 5);
    pub const SET_ATTR: Self = Self(1 << 6);
    pub const LIST_PROP: Self = Self(1 << 7);
    pub const GET_PROP: Self = Self(1 << 8);
    pub const SET_PROP: Self = Self(1 << 9);
    pub const LIST: Self = Self(1 << 10);
    pub const ADD: Self = Self(1 << 11);
    pub const REMOVE: Self = Self(1 << 12);
    pub const HIDE: Self = Self(1 << 13);
    pub const SHOW: Self = Self(1 << 14);
    pub const BLEND: Self = Self(1 << 15);
    pub const USE: Self = Self(1 << 16);
    pub const RECEIVE: Self = Self(1 << 17);
    pub const INSTALL: Self = Self(1 << 18);
    pub const UNINSTALL: Self = Self(1 << 19);

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

/// X resource classes exposed to resource access policies.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResourceKind {
    Window,
    Pixmap,
    Gc,
    Font,
    Cursor,
    Colormap,
    Picture,
    GlyphSet,
    Region,
    PointerBarrier,
    SyncCounter,
    SyncAlarm,
    SyncFence,
    Damage,
    PresentEventSelection,
    ShmSegment,
    GlxContext,
    GlxDrawable,
    Dri3Syncobj,
    RecordContext,
    RandrOutput,
    RandrCrtc,
    RandrMode,
    RandrProvider,
}

/// Resource ownership metadata supplied when a policy checks a creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Creation {
    pub parent: Option<ResourceId>,
    pub owner: ClientId,
}

/// The typed X resource a policy is asked about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceTarget {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub creation: Option<Creation>,
    /// Original owner of a retained resource instance whose XID may have
    /// been reused by another live resource.
    pub owner: Option<ClientId>,
}

/// The request opcodes active at an access check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestOpcodes {
    pub major: u8,
    pub minor: u8,
}

impl RequestOpcodes {
    /// The opcode pair Xorg exposes while dispatching this request.
    /// Core requests use minor opcode zero; extension requests use the
    /// request's data byte as their minor opcode.
    pub const fn for_request(header: RequestHeader) -> Self {
        Self {
            major: header.opcode,
            minor: if header.opcode < 128 { 0 } else { header.data },
        }
    }
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

    /// Resolve the owner of an object of exactly this resource kind.
    #[must_use]
    pub fn owner(&self, id: ResourceId, kind: ResourceKind) -> Option<ClientId> {
        self.state.access_resource_owner(id, kind)
    }

    /// Resolve the owner associated with this particular target instance.
    /// Retained resources can outlive their XID, which may now name a
    /// different live object.
    #[must_use]
    pub fn target_owner(&self, target: &ResourceTarget) -> Option<ClientId> {
        target.owner.or_else(|| self.owner(target.id, target.kind))
    }

    /// Whether `window` is this screen's root window.
    #[must_use]
    pub fn is_root(&self, window: ResourceId) -> bool {
        window == crate::resources::ROOT_WINDOW
    }

    /// Resolve an atom's current name without interning it.
    #[must_use]
    pub fn atom_name(&self, atom: AtomId) -> Option<&str> {
        self.state.atoms.name(atom)
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

    /// Decide whether an available extension is visible to a client during
    /// `QueryExtension` or `ListExtensions` discovery.
    fn extension_access(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        _extension: &str,
    ) -> Decision {
        Decision::Allow
    }

    /// Decide whether a request may be dispatched to an extension handler.
    /// The request opcodes in `ctx` identify the operation being checked.
    fn extension_dispatch(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        _extension: &str,
    ) -> Decision {
        Decision::Allow
    }

    /// Decide whether a client may perform `mode` on a typed resource.
    fn resource(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        _target: &ResourceTarget,
        _mode: AccessMode,
    ) -> Decision {
        Decision::Allow
    }

    /// Decide whether a client may perform `mode` on a window property.
    fn property(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        _window: ResourceId,
        _property: AtomId,
        _mode: AccessMode,
    ) -> Decision {
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
