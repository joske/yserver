//! Rootless mode: every top-level X window becomes a native window of a host
//! (spec `docs/superpowers/specs/2026-10-19-rootless-core-design.md`).
//!
//! Off unless an embedder calls [`enable`]; every hook returns at once when
//! `ServerState::rootless` is `None`.

mod acks;
mod host;
mod info;
mod lock;
mod seat;
pub mod selection;
mod topology;
mod types;
mod wm;

#[cfg(test)]
mod tests;

pub use info::{
    Decorations, InfoFields, NetWmState, Protocols, SizeHints, WindowInfo, WindowType,
    parse_window_info,
};
pub use types::{
    Geometry, GeometryFields, HostOutput, HostRequest, HostSerial, Layer, Rect, ResizeEdge,
    RootlessEvent, RootlessInput, ToplevelId, ToplevelKind, WmRequest,
};

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU64,
};

use yserver_protocol::x11::ResourceId;

pub use acks::ConfigureSource;
pub(crate) use acks::note_configure;
pub(crate) use lock::note_warp;
pub use seat::PointerScope;
pub(crate) use seat::map_input;
pub use selection::{SelectionError, SelectionEvent};
pub(crate) use wm::{note_root_client_message, refuse_root_redirect};

use crate::{
    backend::Backend,
    resources::{ROOT_WINDOW, SERVER_OWNER},
    server::{CompositeRedirectMode, RedirectRecord, ServerState},
};

/// How native positions relate to X positions (spec §5). Scale stays host-side (§4.0).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoordinatePolicy {
    Global { origin: (i32, i32) },
    Opaque,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootlessError {
    AlreadyEnabled,
    /// The backend cannot give redirected windows a backing.
    NoRedirectActivation,
    /// A client already holds a Manual redirect on the root's children.
    ManualTaken,
}

impl std::fmt::Display for RootlessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyEnabled => f.write_str("rootless mode is already enabled"),
            Self::NoRedirectActivation => {
                f.write_str("the backend does not support redirect backings")
            }
            Self::ManualTaken => f.write_str("a client holds a Manual redirect on the root"),
        }
    }
}

impl std::error::Error for RootlessError {}

/// Rootless mode state, held in `ServerState::rootless`.
#[derive(Debug)]
pub struct Rootless {
    policy: CoordinatePolicy,
    /// Next `ToplevelId`; never reused, also across a server reset.
    next_id: NonZeroU64,
    /// Mapped top-levels the host knows about.
    toplevels: HashMap<ResourceId, topology::Tracked>,
    /// `toplevels` in X stacking order, bottom → top.
    order: Vec<ResourceId>,
    /// Live ids only; host input naming any other id is dropped.
    live: HashMap<ToplevelId, ResourceId>,
    /// Windows whose info properties changed since the last reconcile.
    dirty_props: HashSet<ResourceId>,
    /// Events queued outside reconcile; drained first.
    pending: Vec<RootlessEvent>,
    /// Root ClientMessages for the WM, by target window, in arrival order.
    pending_wm: Vec<(ResourceId, WmRequest)>,
    /// Managed windows whose mapping ended this reconcile (`WM_STATE` Withdrawn).
    withdrawn: Vec<ResourceId>,
    /// `_NET_CLIENT_LIST`: Managed windows in initial map order.
    client_list: Vec<ResourceId>,
    written_client_list: Vec<u32>,
    written_stacking: Vec<u32>,
    /// Pointer scope for the hit test (spec §4.4).
    scope: PointerScope,
    /// The Managed top-level the host last activated: where its keyboard is.
    active: Option<ResourceId>,
    /// The pointer lock (`lock.rs`).
    lock: lock::LockState,
    /// What was sent the server's selection window, and the windows the
    /// host watches (`selection.rs`).
    selection: selection::SelectionBridge,
    /// The window manager's name (`name_window_manager`).
    wm_name: Option<String>,
}

impl Rootless {
    fn new(policy: CoordinatePolicy) -> Self {
        Self {
            policy,
            next_id: NonZeroU64::MIN,
            toplevels: HashMap::new(),
            order: Vec::new(),
            live: HashMap::new(),
            dirty_props: HashSet::new(),
            pending: Vec::new(),
            pending_wm: Vec::new(),
            withdrawn: Vec::new(),
            client_list: Vec::new(),
            written_client_list: Vec::new(),
            written_stacking: Vec::new(),
            scope: PointerScope::Anywhere,
            active: None,
            lock: lock::LockState::default(),
            selection: selection::SelectionBridge::default(),
            wm_name: None,
        }
    }

    /// The state a new server generation starts from: the policy and the id
    /// counter survive, so no `ToplevelId` is reused.
    pub(crate) fn for_next_generation(&self) -> Self {
        Self {
            next_id: self.next_id,
            wm_name: self.wm_name.clone(),
            ..Self::new(self.policy)
        }
    }

    #[must_use]
    pub fn policy(&self) -> CoordinatePolicy {
        self.policy
    }

    #[must_use]
    pub fn pointer_scope(&self) -> PointerScope {
        self.scope
    }

    /// The live id of a mapped top-level.
    #[must_use]
    pub fn toplevel_id(&self, window: ResourceId) -> Option<ToplevelId> {
        self.toplevels.get(&window).map(|t| t.id)
    }

    /// This iteration's events (spec §4.2.1 rule 7: nothing unchanged is emitted).
    fn reconcile(&mut self, state: &ServerState) -> Vec<RootlessEvent> {
        let mut events = std::mem::take(&mut self.pending);
        events.extend(self.reconcile_topology(state));
        events
    }
}

/// Turn rootless mode on.
///
/// # Errors
///
/// [`RootlessError::AlreadyEnabled`] when the mode is already on,
/// [`RootlessError::NoRedirectActivation`] when the backend cannot give
/// top-levels a backing, [`RootlessError::ManualTaken`] when a client already
/// holds Manual on the root's children.
pub fn enable(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    policy: CoordinatePolicy,
) -> Result<(), RootlessError> {
    if state.rootless.is_some() {
        return Err(RootlessError::AlreadyEnabled);
    }
    if !backend.supports_redirect_activation() {
        return Err(RootlessError::NoRedirectActivation);
    }
    redirect_root_children(state, backend)?;
    wm::announce(state, backend);
    state.rootless = Some(Rootless::new(policy));
    Ok(())
}

/// Re-install rootless mode on a fresh server generation (reset step 7).
pub(crate) fn resume(state: &mut ServerState, backend: &mut dyn Backend, rootless: Rootless) {
    match redirect_root_children(state, backend) {
        Ok(()) => {
            wm::announce(state, backend);
            if let Some(name) = rootless.wm_name.clone() {
                wm::set_wm_name(state, backend, &name);
            }
            state.rootless = Some(rootless);
        }
        Err(e) => log::error!("rootless: cannot resume after reset: {e}"),
    }
}

/// The server redirects every root child Manual, as the Composite request
/// path does for a client (`composite_damage.rs`), with no client. Later
/// children get theirs at creation (`redirect_new_subwindow`).
fn redirect_root_children(
    state: &mut ServerState,
    backend: &mut dyn Backend,
) -> Result<(), RootlessError> {
    use crate::core_loop::process_request::{
        BackingDamage, subwindows_redirect_targets, sync_redirect_backing,
    };
    let targets = subwindows_redirect_targets(state, ROOT_WINDOW);
    let before: Vec<_> = targets
        .iter()
        .map(|t| state.composite_redirects.window_mode(*t))
        .collect();
    let record = RedirectRecord {
        mode: CompositeRedirectMode::Manual,
        owner: SERVER_OWNER,
    };
    state
        .composite_redirects
        .redirect_subwindows(ROOT_WINDOW, &targets, record)
        .map_err(|_| RootlessError::ManualTaken)?;
    for (target, before) in targets.into_iter().zip(before) {
        sync_redirect_backing(
            state,
            backend,
            None,
            target,
            before,
            BackingDamage::Allocation,
        );
    }
    Ok(())
}

/// Name the built-in window manager `name` rather than "yserver" in its
/// `_NET_SUPPORTING_WM_CHECK` window's `_NET_WM_NAME` (EWMH §3.13): the host
/// manages the windows, and a client that shows its window manager (Steam's
/// "X Window Manager") names the host's. Kept across a server reset.
/// No-op when rootless is off.
pub fn name_window_manager(state: &mut ServerState, backend: &mut dyn Backend, name: &str) {
    let Some(rootless) = state.rootless.as_mut() else {
        return;
    };
    rootless.wm_name = Some(name.to_owned());
    wm::set_wm_name(state, backend, name);
}

/// A property of `window` changed (the `props.rs` writers): re-parse its
/// info at the next reconcile. No-op when rootless is off.
pub(crate) fn note_property_changed(
    state: &mut ServerState,
    window: ResourceId,
    property: yserver_protocol::x11::AtomId,
) {
    let Some(rootless) = state.rootless.as_mut() else {
        return;
    };
    if state
        .atoms
        .name(property)
        .is_some_and(|n| info::INFO_PROPERTIES.contains(&n))
    {
        rootless.dirty_props.insert(window);
    }
}

/// A host request (`Message::Rootless`), applied on the core thread before
/// the tail's reconcile acknowledges it (spec §4.1).
pub fn host_request(state: &mut ServerState, backend: &mut dyn Backend, request: HostRequest) {
    host::apply(state, backend, request);
}

/// Reconcile and hand the events to the backend. Called from the loop tail
/// between the Present drain and `maybe_composite` (spec §4.1).
pub fn reconcile_and_dispatch(state: &mut ServerState, backend: &mut dyn Backend) {
    wm::place_transients(state, backend);
    let Some(mut rootless) = state.rootless.take() else {
        return;
    };
    let mut events = rootless.reconcile(state);
    if let Some(event) = lock::reconcile(&mut rootless, state, backend.displayed_cursor_hidden()) {
        events.push(event);
    }
    wm::apply_policy(state, backend, &mut rootless, &events);
    state.rootless = Some(rootless);
    if !events.is_empty() {
        backend.on_rootless_events(state, &events);
    }
}

#[cfg(test)]
pub(crate) fn queue_event_for_test(state: &mut ServerState, event: RootlessEvent) {
    state
        .rootless
        .as_mut()
        .expect("rootless enabled")
        .pending
        .push(event);
}
