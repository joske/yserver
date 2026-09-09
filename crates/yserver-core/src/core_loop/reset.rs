//! The server-reset generation boundary
//! (`docs/superpowers/specs/2026-09-09-server-reset-design.md`, "The
//! generation boundary" / "Seeding the new generation" / "Forced
//! cleanup, not the normal disconnect path").
//!
//! Three pieces:
//!
//! - [`ResetPolicy`] / [`ResetTrigger`] — the `-noreset` / `-reset` /
//!   `-terminate` policy and the armed trigger that decides *when* the
//!   boundary is crossed ("The trigger must be armed, not inferred").
//! - [`force_destroy_all_clients`] — the forced session teardown, which
//!   destroys every client ignoring close-down mode and releases what
//!   they hold on BOTH sides.
//! - [`reset_generation`] — the boundary itself, which assembles that
//!   with the generation counter, the setup registry, the loop-local
//!   per-client collections and a freshly seeded `ServerState`.

use std::{collections::VecDeque, os::fd::AsRawFd};

use mio::unix::SourceFd;
use yserver_protocol::x11::ClientId;

use crate::{
    backend::{Backend, BackendTopology, PixmapHandle, install_backend_root_bindings},
    core_loop::{
        Generation, GenerationCounter, InputInventory,
        process_disconnect::{
            HostPixmapFrees, destroy_zombie_resources_reporting, process_disconnect_reporting,
        },
        run::{
            DeferredRequest, FairRequestQueue, LoopTelemetry, PendingBackendRequests,
            cancel_all_pending_backend_requests,
        },
        setup_thread::{self, SetupRegistry},
    },
    resources::ROOT_WINDOW,
    server::ServerState,
};

/// What the server does when the last established client of a
/// generation goes away (`docs/superpowers/specs/2026-09-09-server-
/// reset-design.md`, "Flags and signals").
///
/// The default is [`ResetPolicy::NoReset`], which inverts Xorg
/// (`dispatchExceptionAtReset`, `dix/dispatch.c:3480`, defaults to
/// `DE_RESET`). `starty` and every `just *-hw` recipe launch the server
/// expecting it to outlive its clients, so inheriting Xorg's default
/// would turn a momentarily empty client set into what looks exactly
/// like a crash. The divergence is in the safe direction: wrongly not
/// resetting leaves a stale session, wrongly resetting destroys a live
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResetPolicy {
    /// `-noreset`: never reset. Behaviour is byte-identical to a server
    /// built before this feature existed.
    #[default]
    NoReset,
    /// `-reset`: cross the generation boundary when the session drains.
    Reset,
    /// `-terminate`: exit the process cleanly instead of resetting.
    Terminate,
}

/// What the loop must do once it reaches the end of the iteration a
/// trigger fired in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetAction {
    /// Call [`reset_generation`] and carry on serving.
    Reset,
    /// Return from `run_core`, shutting the server down cleanly.
    Terminate,
}

/// The armed reset trigger for the generation currently running.
///
/// Xorg's trigger is an **event** inside `CloseDownClient`
/// (`dix/dispatch.c:3537`) with two conditions — the departing client
/// reached `ClientStateRunning`, and the count is now zero — and this
/// mirrors it explicitly. A state check of the shape "no clients are
/// connected" would fire on an idle `-reset` server before anyone had
/// ever connected, so the departure is what latches an action here;
/// nothing else in the loop may synthesise one.
pub(crate) struct ResetTrigger {
    policy: ResetPolicy,
    /// False at the start of every generation. Set only where a client
    /// becomes *established* — not at accept, not at connect — so a
    /// port scan, a dropped handshake or a refused cookie arms nothing.
    armed: bool,
    /// Latched by a SIGHUP request. Not cancellable: the operator asked
    /// for it, and a connection racing the logout must not veto it.
    forced: bool,
    /// Latched by a departure that drained an armed generation.
    /// Cancelled if a client becomes established before the loop
    /// reaches the boundary, because the session is then not drained
    /// after all.
    drained: Option<ResetAction>,
}

impl ResetTrigger {
    pub(crate) fn new(policy: ResetPolicy) -> Self {
        Self {
            policy,
            armed: false,
            forced: false,
            drained: None,
        }
    }

    /// The policy this trigger was built with.
    #[cfg(test)]
    pub(crate) fn policy(&self) -> ResetPolicy {
        self.policy
    }

    /// Whether this generation has ever seen a client become
    /// established.
    #[cfg(test)]
    pub(crate) fn is_armed(&self) -> bool {
        self.armed
    }

    /// A client became established (`state.clients.insert` in
    /// `handle_client_setup_complete`). Arms the trigger, and cancels a
    /// drain latched earlier in this same iteration — the session has a
    /// client again, so it is no longer drained. A SIGHUP request is
    /// deliberately *not* cancelled.
    pub(crate) fn note_client_established(&mut self) {
        self.armed = true;
        self.drained = None;
    }

    /// A disconnect completed, leaving `clients_remaining` established
    /// clients. The only site that may latch a drain.
    ///
    /// `RetainPermanent` inhibits nothing: `process_disconnect` removes
    /// the `state.clients` entry whatever the close-down mode, so a
    /// retained client is gone for this count even though its resources
    /// survive as a zombie.
    pub(crate) fn note_client_departed(&mut self, clients_remaining: usize) {
        if !self.armed || clients_remaining != 0 {
            return;
        }
        self.drained = match self.policy {
            // Not "latch and ignore later" — nothing at all, so the
            // default server behaves exactly as it did before resets
            // existed.
            ResetPolicy::NoReset => return,
            ResetPolicy::Reset => Some(ResetAction::Reset),
            ResetPolicy::Terminate => Some(ResetAction::Terminate),
        };
    }

    /// SIGHUP arrived as [`Message::ResetRequested`].
    ///
    /// [`Message::ResetRequested`]: crate::core_loop::Message::ResetRequested
    pub(crate) fn note_reset_requested(&mut self) {
        if self.policy == ResetPolicy::NoReset {
            // Unreachable in production — the signal thread sends
            // `Shutdown` under `-noreset` and never produces this
            // message — but honouring it here anyway would break the
            // "byte-identical to today" guarantee for anything that
            // sends it by another route.
            log::warn!("reset: ignoring a reset request under -noreset");
            return;
        }
        // A forced reset outranks `-terminate`: Xorg's `AutoResetServer`
        // raises `DE_RESET`, not `DE_TERMINATE`, and the spec says
        // SIGHUP forces a reset regardless of policy.
        self.forced = true;
    }

    /// Take whatever this iteration latched, if anything.
    pub(crate) fn take_pending(&mut self) -> Option<ResetAction> {
        if self.forced {
            self.forced = false;
            self.drained = None;
            return Some(ResetAction::Reset);
        }
        self.drained.take()
    }

    /// Start a fresh generation: disarmed again, so the empty client
    /// set the reset leaves behind cannot fire a second reset.
    pub(crate) fn begin_generation(&mut self) {
        self.armed = false;
        self.forced = false;
        self.drained = None;
    }
}

/// Destroy every client of the current session — live and zombie — ignoring
/// close-down mode, and release what they hold on BOTH sides.
///
/// Two things separate this from the normal disconnect path:
///
/// 1. `process_disconnect` honours close-down mode on purpose (`let retain =
///    close_mode == 1 || close_mode == 2`), keeping a `RetainPermanent` /
///    `RetainTemporary` client's resources alive as a zombie. A reset erases
///    the session those resources were retained *in*, so the mode is dropped
///    before the disconnect runs and every client takes the destroy branch.
/// 2. Replacing `ServerState` afterwards would drop the *core* metadata while
///    leaving host pixmaps, GLX export refs, DRI3 syncobjs and host-window
///    registrations allocated with nothing left to reference them — the #133
///    lifetime class. So the release calls happen here, through the same
///    `host_xid_still_referenced` orphan gate the disconnect path uses.
pub fn force_destroy_all_clients(state: &mut ServerState, backend: &mut dyn Backend) {
    let mut frees = HostPixmapFrees::default();

    // Sorted so the teardown order — and therefore which client's free sees a
    // still-live reference — is reproducible rather than HashMap-random.
    let mut live: Vec<u32> = state.clients.keys().copied().collect();
    live.sort_unstable();
    for id in live {
        // Dropping the mode entry before the call is what makes this forced:
        // `process_disconnect` reads and removes it itself, and an absent
        // entry reads as DestroyAll.
        state.close_down_modes.remove(&id);
        process_disconnect_reporting(state, backend, ClientId(id), Some(&mut frees));
    }

    // Zombies parked by *earlier* disconnects in this session. They have no
    // `state.clients` entry, so the loop above never reaches them; their
    // resources and host storage are reachable only from here.
    let mut zombies: Vec<u32> = state.zombie_clients.keys().copied().collect();
    zombies.sort_unstable();
    for id in zombies {
        destroy_zombie_resources_reporting(state, backend, ClientId(id), Some(&mut frees));
    }
    state.zombie_clients.clear();
    state.close_down_modes.clear();

    // Re-examine every host pixmap a per-client teardown held back. The gate
    // is per-reference, not per-client: client A's tile stays allocated while
    // client B's GC or window border still names it. In a normal disconnect
    // that is the whole answer — B is still running. In a reset B is destroyed
    // too, and nothing reports the tile a second time (a GC teardown yields no
    // freeable candidates), so without this sweep it survives the session that
    // owned it.
    let freed: std::collections::BTreeSet<u32> = frees.freed.into_iter().collect();
    let mut leftovers: Vec<u32> = frees
        .deferred
        .into_iter()
        .filter(|xid| !freed.contains(xid))
        .collect();
    leftovers.sort_unstable();
    leftovers.dedup();
    for xid in leftovers {
        if PixmapHandle::from_raw(xid)
            .is_some_and(|handle| state.resources.host_xid_still_referenced(handle))
        {
            // Still named by something that outlives the session — the root
            // window's background, say. Not ours to free.
            continue;
        }
        let _ = backend.free_pixmap(None, xid);
    }
}

/// The loop-local per-client collections that live OUTSIDE
/// `ServerState` and therefore survive a state swap.
///
/// Grouped into one struct so [`reset_generation`] names them all at
/// its call site: the hazard the plan calls out is not clearing them
/// wrongly, it is *forgetting* one, and a struct makes the set
/// reviewable in a single place.
pub(crate) struct GenerationLocals<'a> {
    /// The fair round-robin request queue (`by_client` / `ready`).
    pub deferred_requests: &'a mut FairRequestQueue,
    /// The SEPARATE server-grab waiter queue. Not part of the fair
    /// queue, and the one with teeth: `release_server_grab_waiters`
    /// pushes its contents back into `deferred_requests` the moment a
    /// server grab releases, so anything left here would be restored
    /// into the fresh generation.
    pub server_grab_waiters: &'a mut VecDeque<DeferredRequest>,
    /// Parked asynchronous CRTC configurations, indexed by token and by
    /// client.
    pub pending_backend_requests: &'a mut PendingBackendRequests,
    /// Per-client loop telemetry rows. Diagnostics only; see
    /// `LoopTelemetry::forget_clients` for why a reset has to clear
    /// them explicitly.
    pub telemetry: &'a mut LoopTelemetry,
}

/// Cross the generation boundary: quarantine the old session, destroy
/// it, and install a freshly seeded `ServerState` in its place.
///
/// Returns the new generation. The steps are the spec's, in the spec's
/// order (`docs/superpowers/specs/2026-09-09-server-reset-design.md`,
/// "The generation boundary"); each is marked below.
///
/// Called from `run_core` when [`ResetTrigger::take_pending`] yields
/// [`ResetAction::Reset`]. It never exits the process, never
/// re-initialises KMS or Vulkan, and never touches `listeners`.
pub(crate) fn reset_generation(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    registry: &mio::Registry,
    generations: &GenerationCounter,
    setup_registry: &SetupRegistry,
    inventory: &InputInventory,
    locals: GenerationLocals<'_>,
) -> Generation {
    // -- 1. Bump the generation. ------------------------------------
    // First, so everything below is defined relative to the new one and
    // any message a still-running producer tags from here on is already
    // stale by construction.
    let generation = generations.bump();

    // -- 2. Cancel pending setup handshakes. ------------------------
    // A handshake that began before the reset would otherwise complete
    // into the NEW generation (`handle_client_setup_complete` inserts
    // into whatever state is current), producing a client authorized
    // against the destroyed session.
    setup_thread::shutdown_all(setup_registry);

    // -- 3. Force-close every established client. -------------------
    // Deregister first: `epoll_ctl(DEL)` needs a live fd, and the
    // teardown below drops the last `ClientState` reference to the
    // writer, closing it.
    let mut live: Vec<u32> = state.clients.keys().copied().collect();
    live.sort_unstable();
    for id in &live {
        let Some(client) = state.clients.get(id) else {
            continue;
        };
        let raw = match client.writer.lock() {
            Ok(writer) => writer.as_raw_fd(),
            Err(poisoned) => poisoned.into_inner().as_raw_fd(),
        };
        if let Err(err) = registry.deregister(&mut SourceFd(&raw))
            && err.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("reset: deregister client {id} from the poller: {err}");
        }
    }
    force_destroy_all_clients(state, backend);

    // -- 4. Clear the per-client state that lives outside ServerState.
    // Cancel BEFORE clearing: the token is the only handle to the
    // backend operation, so emptying the maps first leaks any parked
    // config permanently. `drain_ready_crtc_configs` cannot recover it
    // either -- that path runs only when a completion arrives, and a
    // parked op may never deliver one.
    cancel_all_pending_backend_requests(backend, locals.pending_backend_requests);
    locals.deferred_requests.clear();
    locals.server_grab_waiters.clear();
    locals.telemetry.forget_clients();

    // NOT handled here: the leaked composite-overlay claim. A compositor
    // that disconnects without `ReleaseOverlayWindow` leaves
    // `cow_refcount` held, and a reset would otherwise carry that across
    // the session boundary. The first attempt at this was a bounded
    // decrement loop here, which jos and codex both rejected: the cap is
    // arbitrary and protocol-invalid (a client may issue more Gets than
    // the cap), and on a `materialize_direct_shadow_for_unflip` failure
    // it degenerated to logging and continuing — carrying the old
    // compositor's claim into the next user's session, the exact outcome
    // a reset must forbid.
    //
    // The real fix is structural and belongs upstream of reset: make the
    // claim a PER-CLIENT resource as Xorg does (`FreeCompositeClientOverlay`,
    // ../xserver/composite/compext.c:88, is a resource destructor calling
    // compFreeOverlayClient), share one release helper between
    // ReleaseOverlayWindow and disconnect, and then reset inherits the
    // cleanup through `force_destroy_all_clients` with no special case.
    // Tracked separately; see the spec's "Adjacent gaps".

    // -- 6. Replace `*state` with a freshly seeded one. -------------
    // Constructed, not cleared: a field added to `ServerState` later is
    // then reset correctly by default. Topology and capabilities are
    // re-derived from the LIVE backend through the same snapshot type
    // startup uses, so the second generation is built exactly the way
    // the first one was.
    let topology = BackendTopology::from_backend(backend);
    let mut fresh = topology.into_server_state();
    // The single field that survives literally. X11 timestamps must not
    // go backwards: a client reconnecting a millisecond after a reset
    // would otherwise see the server clock jump.
    fresh.start_instant = state.start_instant;
    // Input is seeded from the process-lifetime inventory, not
    // re-probed: `probe_input_devices` is a no-op in Direct mode
    // (libinput's enumeration burst is one-shot, at process start), so
    // a generation that re-probed would come back with no devices at
    // all. Property-name atoms are interned HERE, against the fresh
    // table -- carrying `xi_devices` across instead would leave every
    // device property pointing at an atom id that no longer exists.
    for info in inventory.devices_by_node() {
        fresh.xi_seed_touchpad(info);
    }
    *state = fresh;

    // -- 7. Re-attach the backend to the new state. -----------------
    install_backend_root_bindings(state, backend);

    // -- 8. Repaint the root. ---------------------------------------
    // The fresh constructor creates a root window, but that leaves the
    // previous session's pixels on screen -- a visual bug, and under
    // XDMCP an information leak to the next user.
    clear_root(state, backend);
    backend.mark_dirty();

    // -- 9. Listeners are left bound and untouched. -----------------
    generation
}

/// Paint the fresh root window's background over the whole screen.
///
/// Deliberately the same call the protocol's `ClearArea` makes, against
/// the same resolved background, so KMS records the damage its
/// composite path needs rather than relying on `mark_dirty` alone.
fn clear_root(state: &mut ServerState, backend: &mut dyn Backend) {
    let Some((width, height)) = state
        .resources
        .window(ROOT_WINDOW)
        .map(|root| (root.width, root.height))
    else {
        return;
    };
    // `None` here means background None -- "leave the contents
    // untouched" -- which a fresh root never is (it is created with a
    // background pixel), but the branch is kept rather than unwrapped
    // so a future default change degrades to "no paint" instead of a
    // panic.
    let Some(background) = state.resources.window_resolved_background(ROOT_WINDOW) else {
        return;
    };
    let Some(target) = state.resources.host_drawable_target(ROOT_WINDOW) else {
        return;
    };
    if let Err(err) = backend.clear_area(
        None,
        target.host_xid(),
        background.background_pixel,
        background
            .background_pixmap_host_xid
            .map(crate::backend::PixmapHandle::as_raw),
        0,
        0,
        width,
        height,
        background.tile_origin_offset,
    ) {
        log::warn!("reset: clearing the root window failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    use yserver_protocol::x11::{
        ClientByteOrder, ClientId, CreatePixmapRequest, CreateWindowRequest, ResourceId,
    };

    use super::{
        GenerationLocals, ResetAction, ResetPolicy, ResetTrigger, force_destroy_all_clients,
        reset_generation,
    };
    use crate::{
        backend::{
            Backend, CrtcConfigToken, PixmapHandle, WindowHandle,
            recording::{RecordedCall, RecordingBackend},
        },
        core_loop::{
            GenerationCounter, InputInventory,
            message::{BoolSetting, DeviceInfo, LibinputConfigSnapshot},
            run::{
                FairRequestQueue, LoopTelemetry, PendingBackendRequests, deferred_request_for_test,
                drain_ready_crtc_configs, release_server_grab_waiters,
            },
            setup_thread,
        },
        randr::{RandrMode, RandrOutput},
        resources::ROOT_WINDOW,
        server::{ClientState, GlxContext, GlxDrawable, GlxDrawableKind, ServerState},
    };

    fn install_client(state: &mut ServerState, id: u32) {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: id << 20,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
    }

    /// Everything one client of a session holds that costs the BACKEND
    /// something: a host window, a host pixmap, a GLX drawable holding an
    /// export ref, and a DRI3 syncobj.
    struct SessionFixture {
        window: ResourceId,
        host_window: u32,
        host_pixmap: u32,
        glx_export: u32,
        syncobj: u32,
    }

    fn seed_client_session(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        id: u32,
    ) -> SessionFixture {
        let client = ClientId(id);
        let base = id << 20;
        install_client(state, id);

        // A mapped top-level with a real host window registered in the
        // backend's xid map.
        let window = ResourceId(base | 0x01);
        state.resources.create_window(
            client,
            CreateWindowRequest {
                depth: 24,
                window,
                parent: ROOT_WINDOW,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let host_window = 0x0100_0000 | base;
        backend
            .register_top_level(None, window, host_window)
            .expect("register_top_level");
        state.resources.window_mut(window).expect("window").host_xid =
            Some(WindowHandle::from_raw_for_test(host_window));

        // A pixmap resource backed by real backend storage.
        let pixmap = ResourceId(base | 0x02);
        let host_pixmap = backend
            .create_pixmap(None, 24, 16, 16)
            .expect("create_pixmap")
            .as_raw();
        state.resources.create_pixmap(
            client,
            CreatePixmapRequest {
                pixmap,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(
            pixmap,
            PixmapHandle::from_raw(host_pixmap).expect("nonzero")
        ));

        // A GLXPixmap holding an export-lifetime ref on that storage, plus a
        // GLX context.
        backend.acquire_glx_pixmap_export(host_pixmap);
        state.glx_drawables.insert(
            base | 0x03,
            GlxDrawable {
                owner: client,
                kind: GlxDrawableKind::Pixmap,
                x_drawable: pixmap.0,
                fbconfig: 0x21,
                width: 16,
                height: 16,
                event_mask: 0,
                glx_export_host_xid: Some(host_pixmap),
            },
        );
        state.glx_contexts.insert(
            base | 0x04,
            GlxContext {
                owner: client,
                fbconfig: 0x21,
                render_type: 0x8014,
            },
        );

        // A DRI3 syncobj, tracked in core AND in the backend's own registry.
        let syncobj = base | 0x05;
        assert!(
            state
                .resources
                .register_dri3_syncobj(ResourceId(syncobj), client)
        );
        backend.seed_dri3_syncobj_for_test(syncobj, client);

        SessionFixture {
            window,
            host_window,
            host_pixmap,
            glx_export: host_pixmap,
            syncobj,
        }
    }

    /// The load-bearing assertion for the whole step: after a forced
    /// teardown the BACKEND holds nothing. A `ServerState`-only check passes
    /// with no release calls at all, so it would prove nothing here.
    #[test]
    fn force_destroy_all_clients_empties_backend_accounting() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let a = seed_client_session(&mut state, &mut backend, 7);
        let b = seed_client_session(&mut state, &mut backend, 8);

        assert_eq!(backend.live_pixmaps.len(), 2, "fixture seeded two pixmaps");
        assert_eq!(backend.glx_pixmap_exports.len(), 2);
        assert_eq!(backend.dri3_syncobj_owners.len(), 2);

        force_destroy_all_clients(&mut state, &mut backend);

        assert!(
            backend.live_pixmaps.is_empty(),
            "host pixmaps still allocated after a forced teardown: {:?}",
            backend.live_pixmaps
        );
        assert!(
            backend.glx_pixmap_exports.is_empty(),
            "GLX export refs still held after a forced teardown: {:?}",
            backend.glx_pixmap_exports
        );
        assert!(
            backend.dri3_syncobj_owners.is_empty(),
            "DRI3 syncobjs still imported after a forced teardown"
        );
        assert!(
            backend.xid_map().is_empty(),
            "host windows still registered after a forced teardown: {:?}",
            backend.xid_map()
        );

        // Core side, for completeness — this half is what passes trivially.
        assert!(state.clients.is_empty());
        assert!(state.zombie_clients.is_empty());
        assert!(state.close_down_modes.is_empty());
        assert!(state.glx_drawables.is_empty());
        assert!(state.glx_contexts.is_empty());
        for f in [&a, &b] {
            assert!(!state.resources.xid_in_use(ResourceId(f.syncobj)));
            assert!(state.resources.window(f.window).is_none());
            assert!(
                !state
                    .resources
                    .host_xid_still_referenced(PixmapHandle::from_raw(f.host_pixmap).unwrap())
            );
            assert_eq!(f.glx_export, f.host_pixmap);
            assert!(backend.xid_map().get(&f.host_window).is_none());
        }
    }

    /// Close-down mode is honoured by `process_disconnect` on purpose
    /// (`process_disconnect.rs`: `let retain = close_mode == 1 || close_mode
    /// == 2`). The reset path must not honour it: a retained resource cannot
    /// outlive the session it was retained in.
    #[test]
    fn force_destroy_all_clients_ignores_retain_permanent() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let f = seed_client_session(&mut state, &mut backend, 7);
        state.close_down_modes.insert(7, 1); // RetainPermanent

        force_destroy_all_clients(&mut state, &mut backend);

        assert!(
            state.zombie_clients.is_empty(),
            "a forced teardown must not leave a zombie behind"
        );
        assert!(
            backend.live_pixmaps.is_empty(),
            "RetainPermanent must not keep host storage alive across a reset: {:?}",
            backend.live_pixmaps
        );
        assert!(backend.dri3_syncobj_owners.is_empty());
        assert!(backend.glx_pixmap_exports.is_empty());
        assert!(backend.xid_map().is_empty());
        assert!(!state.resources.xid_in_use(ResourceId(f.syncobj)));
    }

    /// A client that already disconnected with `RetainPermanent` is a zombie
    /// with no `state.clients` entry. Its resources are reachable only via
    /// `state.zombie_clients`, so a teardown that walks live clients alone
    /// leaves them — and their backend storage — behind.
    #[test]
    fn force_destroy_all_clients_destroys_pre_existing_zombies() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let f = seed_client_session(&mut state, &mut backend, 7);
        state.close_down_modes.insert(7, 1);
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut backend,
            ClientId(7),
        );
        assert_eq!(
            state.zombie_clients.get(&7),
            Some(&1),
            "precondition: the retain path parked a zombie"
        );
        assert_eq!(
            backend.live_pixmaps.len(),
            1,
            "precondition: the retained pixmap's storage survived the disconnect"
        );

        force_destroy_all_clients(&mut state, &mut backend);

        assert!(
            backend.live_pixmaps.is_empty(),
            "zombie host storage survived the reset: {:?}",
            backend.live_pixmaps
        );
        assert!(backend.dri3_syncobj_owners.is_empty());
        assert!(state.zombie_clients.is_empty());
        assert!(!state.resources.xid_in_use(ResourceId(f.syncobj)));
    }

    /// The orphan rule is `ResourceTable::host_xid_still_referenced` (#133),
    /// and it is per-reference, not per-client: while client 8's GC still
    /// names client 7's tile, tearing 7 down must NOT free it. Once 8 is gone
    /// too nothing names it, and the reset must not walk away leaving it
    /// allocated — the failure a naive per-client loop produces, because the
    /// deferral is decided before the later client is destroyed.
    #[test]
    fn force_destroy_all_clients_frees_a_tile_deferred_across_clients() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        install_client(&mut state, 8);

        let tile = ResourceId(0x0070_0001);
        let host_tile = backend
            .create_pixmap(None, 24, 8, 8)
            .expect("create_pixmap")
            .as_raw();
        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: tile,
                drawable: ROOT_WINDOW,
                width: 8,
                height: 8,
                depth: 24,
            },
        );
        assert!(
            state
                .resources
                .set_pixmap_host_xid(tile, PixmapHandle::from_raw(host_tile).expect("nonzero"))
        );
        // Client 8 stipples with client 7's tile: a GC reference, which
        // `host_xid_still_referenced` counts and which no window teardown
        // reports back as a freeable candidate.
        state.resources.seed_gc_with_tile_for_test(
            ClientId(8),
            ResourceId(0x0080_0001),
            PixmapHandle::from_raw(host_tile).expect("nonzero"),
        );

        force_destroy_all_clients(&mut state, &mut backend);

        assert!(
            backend.live_pixmaps.is_empty(),
            "a tile deferred while another client's GC held it was never \
             re-examined once that client died: {:?}",
            backend.live_pixmaps
        );
    }
    // ────────────────────────────────────────────────────────────────
    // `reset_generation` — the boundary itself.
    // ────────────────────────────────────────────────────────────────

    /// A poller for the tests. `reset_generation` deregisters every
    /// client fd from it; nothing here has ever been registered, so the
    /// calls take the tolerated `NotFound` branch.
    fn poll() -> mio::Poll {
        mio::Poll::new().expect("mio poll")
    }

    struct Locals {
        deferred_requests: FairRequestQueue,
        server_grab_waiters: VecDeque<super::DeferredRequest>,
        pending_backend_requests: PendingBackendRequests,
        telemetry: LoopTelemetry,
    }

    impl Locals {
        fn new() -> Self {
            Self {
                deferred_requests: FairRequestQueue::default(),
                server_grab_waiters: VecDeque::new(),
                pending_backend_requests: PendingBackendRequests::default(),
                telemetry: LoopTelemetry::default(),
            }
        }

        fn borrow(&mut self) -> GenerationLocals<'_> {
            GenerationLocals {
                deferred_requests: &mut self.deferred_requests,
                server_grab_waiters: &mut self.server_grab_waiters,
                pending_backend_requests: &mut self.pending_backend_requests,
                telemetry: &mut self.telemetry,
            }
        }
    }

    /// Mirrors `xinput::tests::touchpad_info`: tap must be *available*
    /// for `seed_touchpad` to intern `libinput Tapping Enabled`, which
    /// is the property this module's atom assertion turns on.
    fn touchpad(node: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            device_node: node.into(),
            sysname: node.trim_start_matches("/dev/input/").into(),
            vendor_id: 0x046d,
            product_id: 0xc52f,
            is_touchpad: true,
            config: LibinputConfigSnapshot {
                tap: BoolSetting {
                    available: true,
                    current: true,
                    default: false,
                },
                natural_scroll: BoolSetting {
                    available: true,
                    current: false,
                    default: true,
                },
                ..Default::default()
            },
        }
    }

    /// A backend carrying a topology distinguishable from every
    /// `ServerState` default, so "the new generation was re-derived from
    /// the backend" is provable rather than coincidental.
    fn backend_with_topology() -> RecordingBackend {
        let mut backend = RecordingBackend::new();
        backend.fb_size = (1920, 1080);
        backend.randr_outputs = vec![RandrOutput {
            name: "DP-1".to_string(),
            output_id: 0x40,
            crtc_id: 0x41,
            mode_id: 0x42,
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            timing: None,
            mm_width: 520,
            mm_height: 290,
            mode_ids: vec![0x42],
            num_preferred: 1,
        }];
        backend.randr_modes = vec![RandrMode {
            mode_id: 0x42,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            timing: None,
        }];
        backend
    }

    /// The core obligation: after a direct call the state is a fresh
    /// one — no resources, atoms, selections or grabs from the destroyed
    /// session — while `start_instant` (the X11 timestamp epoch) is
    /// carried over literally, and the generation has advanced.
    #[test]
    fn reset_generation_installs_a_fresh_state_and_keeps_the_timestamp_epoch() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        let f = seed_client_session(&mut state, &mut backend, 7);
        let session_atom = state.atoms.intern("_SESSION_ONLY_ATOM", false);
        state.selections.insert(session_atom, (f.window, 1));
        state.server_grab_owner = Some(ClientId(7));
        state.set_pointer_grab(crate::server::ActivePointerGrab {
            owner: ClientId(7),
            grab_window: f.window,
            event_mask: 0,
            cursor: ResourceId(0),
            time: 1,
            owner_events: false,
            via_xi2: false,
            implicit: false,
            passive: false,
            xi2_mask: 0,
        });

        let epoch = state.start_instant;
        let generations = GenerationCounter::new();
        let before = generations.current();
        let registry = setup_thread::make_registry();
        let inventory = InputInventory::new();
        let mut locals = Locals::new();
        let p = poll();

        let generation = reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        assert_ne!(generation, before, "the generation must advance");
        assert_eq!(generations.current(), generation);

        assert!(state.clients.is_empty());
        assert!(state.zombie_clients.is_empty());
        assert!(state.selections.is_empty(), "selections must not survive");
        assert!(state.server_grab_owner.is_none());
        assert!(state.active_pointer_grab.is_none());
        assert!(state.key_grabs.is_empty());
        assert!(state.button_grabs.is_empty());
        assert!(
            !state.resources.xid_in_use(f.window),
            "the destroyed session's window ids must be free again"
        );
        assert!(
            state.atoms.id_for("_SESSION_ONLY_ATOM").is_none(),
            "the atom table must be back to predefined-only"
        );
        assert_ne!(
            state.atoms.name(session_atom),
            Some("_SESSION_ONLY_ATOM"),
            "a session atom id still resolving to its old name is the \
             dangling-atom bug the survive list was corrected for"
        );

        assert_eq!(
            state.start_instant, epoch,
            "start_instant is the ONLY field that survives literally — \
             X11 timestamps must not go backwards across a reset"
        );
    }

    /// Topology is re-derived from the live backend, not carried: the
    /// new root geometry and RandR view come back with the backend's
    /// values, not the destroyed state's.
    #[test]
    fn reset_generation_reseeds_topology_from_the_backend() {
        let mut state = ServerState::with_geometry(640, 480);
        let mut backend = backend_with_topology();
        let generations = GenerationCounter::new();
        let registry = setup_thread::make_registry();
        let inventory = InputInventory::new();
        let mut locals = Locals::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        let root = state.resources.window(ROOT_WINDOW).expect("root");
        assert_eq!(
            (root.width, root.height),
            (1920, 1080),
            "root geometry must come from the backend's live topology"
        );
        assert_eq!(state.randr.screen_width, 1920);
        assert_eq!(
            state.randr.outputs.len(),
            1,
            "the RandR output set must be rebuilt from the backend"
        );
        assert_eq!(state.randr.outputs[0].name, "DP-1");
        assert_eq!(
            root.host_xid.map(|h| h.as_raw()),
            Some(backend.window_id()),
            "install_backend_root_bindings must have re-run against the \
             fresh state"
        );
    }

    /// The devices come back — from the process-lifetime inventory, not
    /// a re-probe — and their property-name atoms are interned in the
    /// NEW atom table. Carrying `xi_devices` instead would leave those
    /// properties pointing at ids the fresh table never issued.
    #[test]
    fn reset_generation_reseeds_devices_with_atoms_in_the_new_table() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        // Burn atom ids in the OLD table so a carried-over property atom
        // would be a recognisably different number from a freshly
        // interned one.
        for i in 0..32 {
            state.atoms.intern(&format!("_OLD_SESSION_{i}"), false);
        }
        let mut inventory = InputInventory::new();
        inventory.add(touchpad("/dev/input/event4", "SynPS/2 Touchpad"));

        let generations = GenerationCounter::new();
        let registry = setup_thread::make_registry();
        let mut locals = Locals::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        let slave = state
            .xi_devices
            .iter()
            .find(|d| d.id == crate::xinput::DEVICEID_SLAVE_POINTER)
            .expect("slave pointer");
        assert_eq!(
            slave.name, "SynPS/2 Touchpad",
            "the device set must be present again after a reset"
        );
        assert!(slave.is_touchpad);

        // `intern(only_if_exists = true)` returns the live id: the
        // property must resolve through the NEW table.
        let tap = state.atoms.intern("libinput Tapping Enabled", true);
        assert_ne!(
            tap,
            yserver_protocol::x11::AtomId(0),
            "the property atom must exist in the new table"
        );
        assert!(
            slave.properties.contains_key(&tap),
            "device properties must be keyed by atoms interned in the new \
             table, not ids carried from the destroyed one"
        );
    }

    /// The quarantine case with teeth. `release_server_grab_waiters`
    /// pushes the waiter queue back into the fair queue whenever a
    /// server grab releases, so a request the destroyed client left
    /// parked there would be restored — and dispatched — inside the
    /// fresh generation.
    #[test]
    fn a_destroyed_clients_server_grab_waiter_is_not_restored_after_the_reset() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        install_client(&mut state, 7);
        let mut locals = Locals::new();
        locals
            .server_grab_waiters
            .push_back(deferred_request_for_test(7));
        locals
            .deferred_requests
            .push_back(deferred_request_for_test(7));

        let generations = GenerationCounter::new();
        let registry = setup_thread::make_registry();
        let inventory = InputInventory::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        assert!(
            locals.server_grab_waiters.is_empty(),
            "the separate server-grab waiter queue must be cleared"
        );
        assert!(locals.deferred_requests.is_empty());

        // Now drive the new generation's grab release, which is the path
        // that would resurrect a leftover waiter.
        release_server_grab_waiters(
            &mut locals.deferred_requests,
            &mut locals.server_grab_waiters,
            &mut locals.telemetry,
        );
        assert!(
            locals.deferred_requests.is_empty(),
            "a destroyed client's request was restored into the fresh \
             generation when its server grab released"
        );
    }

    /// The parked-CRTC token is the only handle to the backend
    /// operation, so the boundary must cancel it before emptying the
    /// maps — and a completion that lands afterwards must be discarded,
    /// not mistaken for a live wait.
    #[test]
    fn a_parked_crtc_config_is_cancelled_and_a_late_completion_is_ignored() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        install_client(&mut state, 7);
        let token = CrtcConfigToken(0x5150);
        let mut locals = Locals::new();
        locals
            .pending_backend_requests
            .park_crtc_for_test(ClientId(7), token)
            .expect("park");

        let generations = GenerationCounter::new();
        let registry = setup_thread::make_registry();
        let inventory = InputInventory::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        assert_eq!(
            backend.cancelled_crtc_configs,
            vec![token],
            "a parked CRTC config must be cancelled at the boundary, not \
             leaked — the token is the only handle to it"
        );
        assert!(
            locals.pending_backend_requests.is_empty(),
            "the parked-CRTC maps must be empty afterwards"
        );

        // A worker completion racing the reset arrives now.
        backend.ready_crtc_configs = vec![token];
        drain_ready_crtc_configs(
            &mut state,
            &mut backend,
            &mut locals.pending_backend_requests,
            &mut ResetTrigger::new(ResetPolicy::NoReset),
        );
        assert!(
            backend.finished_crtc_configs.is_empty(),
            "a late completion for a cancelled token must never be \
             finished into the new generation"
        );
        assert_eq!(
            backend.cancelled_crtc_configs,
            vec![token, token],
            "the late completion is discarded by cancelling again"
        );
    }

    /// Scanout: the reset must repaint the root and wake the compositor.
    /// "The old pixels are actually gone" is step 6, on hardware — here
    /// the obligation is that the clear/dirty path is invoked at all.
    #[test]
    fn reset_generation_clears_the_root_and_marks_the_backend_dirty() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        let generations = GenerationCounter::new();
        let registry = setup_thread::make_registry();
        let inventory = InputInventory::new();
        let mut locals = Locals::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        let calls = backend.calls.lock().expect("calls");
        let root_host_xid = backend.window_id();
        let cleared = calls.iter().find_map(|call| match call {
            RecordedCall::FillRectangle {
                host_xid,
                x,
                y,
                width,
                height,
                ..
            } if *host_xid == root_host_xid => Some((*x, *y, *width, *height)),
            _ => None,
        });
        assert_eq!(
            cleared,
            Some((0, 0, 1920, 1080)),
            "the reset must clear the WHOLE root — at the geometry the \
             fresh state was just built at, not the destroyed one's"
        );

        let dirty = calls
            .iter()
            .position(|call| matches!(call, RecordedCall::MarkDirty));
        let fill = calls
            .iter()
            .position(|call| matches!(call, RecordedCall::FillRectangle { .. }));
        assert!(
            matches!((fill, dirty), (Some(f), Some(d)) if f < d),
            "mark_dirty must follow the clear, or the composite it \
             ungates can run before the repaint: {calls:?}"
        );
    }

    /// Backend-side accounting, not just `ServerState`: the boundary
    /// runs the forced teardown, so host pixmaps, GLX export refs, DRI3
    /// syncobjs and host-window registrations all go with the session.
    #[test]
    fn reset_generation_empties_backend_accounting_for_the_old_session() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        seed_client_session(&mut state, &mut backend, 7);
        seed_client_session(&mut state, &mut backend, 8);
        state.close_down_modes.insert(8, 1); // RetainPermanent
        assert_eq!(backend.live_pixmaps.len(), 2, "precondition");

        let generations = GenerationCounter::new();
        let registry = setup_thread::make_registry();
        let inventory = InputInventory::new();
        let mut locals = Locals::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        assert!(
            backend.live_pixmaps.is_empty(),
            "host pixmaps survived the reset: {:?}",
            backend.live_pixmaps
        );
        assert!(backend.glx_pixmap_exports.is_empty());
        assert!(backend.dri3_syncobj_owners.is_empty());
        assert!(backend.xid_map().is_empty());
    }

    /// The setup registry is emptied at the boundary, so a handshake
    /// still in flight cannot complete into the new generation.
    #[test]
    fn reset_generation_shuts_down_in_flight_setup_handshakes() {
        let mut state = ServerState::new();
        let mut backend = backend_with_topology();
        let registry = setup_thread::make_registry();
        let (a, b) = UnixStream::pair().expect("socketpair");
        registry
            .lock()
            .expect("registry")
            .insert(ClientId(9), crate::transport::Transport::Unix(a));

        let generations = GenerationCounter::new();
        let inventory = InputInventory::new();
        let mut locals = Locals::new();
        let p = poll();

        reset_generation(
            &mut state,
            &mut backend,
            p.registry(),
            &generations,
            &registry,
            &inventory,
            locals.borrow(),
        );

        assert!(
            registry.lock().expect("registry").is_empty(),
            "a pending setup handshake must be cancelled at the boundary"
        );
        // The peer sees EOF: the registry's clone was shut down and
        // dropped, so nothing can still be written to it.
        let mut buf = [0u8; 1];
        assert_eq!(
            std::io::Read::read(&mut &b, &mut buf).expect("read"),
            0,
            "the handshake socket must be closed"
        );
    }

    // ---------------------------------------------------------------
    // The armed trigger (plan step 5, spec "The trigger must be armed,
    // not inferred"). Pure state machine — the loop-level wiring that
    // feeds it is covered by `core_loop::run`'s `server_reset` tests.
    // ---------------------------------------------------------------

    /// One established client, which then leaves.
    fn drained(policy: ResetPolicy) -> ResetTrigger {
        let mut trigger = ResetTrigger::new(policy);
        trigger.note_client_established();
        trigger.note_client_departed(0);
        trigger
    }

    #[test]
    fn the_default_policy_is_noreset() {
        assert_eq!(ResetPolicy::default(), ResetPolicy::NoReset);
        assert_eq!(
            ResetTrigger::new(ResetPolicy::default()).policy(),
            ResetPolicy::NoReset
        );
    }

    #[test]
    fn a_new_trigger_is_not_armed() {
        for policy in [
            ResetPolicy::NoReset,
            ResetPolicy::Reset,
            ResetPolicy::Terminate,
        ] {
            assert!(!ResetTrigger::new(policy).is_armed(), "{policy:?}");
        }
    }

    #[test]
    fn the_last_client_leaving_does_nothing_under_noreset() {
        assert_eq!(drained(ResetPolicy::NoReset).take_pending(), None);
    }

    #[test]
    fn the_last_client_leaving_resets_under_reset() {
        assert_eq!(
            drained(ResetPolicy::Reset).take_pending(),
            Some(ResetAction::Reset)
        );
    }

    #[test]
    fn the_last_client_leaving_terminates_under_terminate() {
        assert_eq!(
            drained(ResetPolicy::Terminate).take_pending(),
            Some(ResetAction::Terminate)
        );
    }

    #[test]
    fn a_departure_that_leaves_another_client_fires_nothing() {
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        trigger.note_client_established();
        trigger.note_client_established();
        trigger.note_client_departed(1);
        assert_eq!(trigger.take_pending(), None);
        trigger.note_client_departed(0);
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
    }

    #[test]
    fn an_idle_reset_server_never_resets() {
        // The case a `clients.is_empty()` state check gets wrong: at
        // startup the client set is ALSO empty, so an unarmed
        // departure-shaped event must fire nothing however often it
        // arrives.
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        for _ in 0..100 {
            trigger.note_client_departed(0);
            assert_eq!(trigger.take_pending(), None);
        }
        assert!(!trigger.is_armed());
    }

    #[test]
    fn a_connection_that_drops_before_completing_setup_arms_nothing() {
        // A handshake that never reaches `handle_client_setup_complete`
        // never calls `note_client_established`; its socket closing is
        // just another unarmed departure.
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        trigger.note_client_departed(0);
        assert!(!trigger.is_armed());
        assert_eq!(trigger.take_pending(), None);
    }

    #[test]
    fn a_client_refused_for_a_bad_cookie_arms_nothing() {
        // Same shape, and the one a stranger can reach: the TCP
        // listener binds 0.0.0.0, so a port scan or a wrong cookie must
        // not be able to make the server erase its session.
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        for _ in 0..10 {
            trigger.note_client_departed(0);
        }
        assert!(!trigger.is_armed());
        assert_eq!(trigger.take_pending(), None);
    }

    #[test]
    fn sighup_forces_a_reset_under_reset() {
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        trigger.note_reset_requested();
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
    }

    #[test]
    fn sighup_forces_a_reset_not_a_terminate_under_terminate() {
        // Xorg's `AutoResetServer` raises DE_RESET, never DE_TERMINATE.
        let mut trigger = ResetTrigger::new(ResetPolicy::Terminate);
        trigger.note_reset_requested();
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
    }

    #[test]
    fn sighup_is_refused_by_the_trigger_under_noreset() {
        // Belt and braces: the production gate is at the sender — the
        // signal thread keeps sending `Shutdown` under `-noreset` — and
        // the trigger refuses the message even if it arrives anyway.
        let mut trigger = ResetTrigger::new(ResetPolicy::NoReset);
        trigger.note_reset_requested();
        assert_eq!(trigger.take_pending(), None);
    }

    #[test]
    fn sighup_forces_a_reset_with_clients_still_connected() {
        // Logout: the point of SIGHUP is to erase a session that is
        // still running, so neither arming nor emptiness gates it.
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        trigger.note_client_established();
        trigger.note_reset_requested();
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
    }

    #[test]
    fn a_client_established_after_the_drain_cancels_it() {
        // The boundary runs at the end of the iteration, so a setup
        // that completes between the disconnect and the boundary
        // un-drains the session; resetting then would destroy a client
        // that had only just connected.
        let mut trigger = drained(ResetPolicy::Reset);
        trigger.note_client_established();
        assert_eq!(trigger.take_pending(), None);
    }

    #[test]
    fn a_client_established_after_a_sighup_does_not_cancel_it() {
        let mut trigger = ResetTrigger::new(ResetPolicy::Reset);
        trigger.note_reset_requested();
        trigger.note_client_established();
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
    }

    #[test]
    fn a_fired_reset_disarms_the_trigger_for_the_new_generation() {
        // The reset leaves an empty client set behind. Without the
        // disarm the next departure-shaped event would reset again —
        // and with the old generation's reader threads still winding
        // down, one is guaranteed to arrive.
        let mut trigger = drained(ResetPolicy::Reset);
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
        trigger.begin_generation();
        assert!(!trigger.is_armed());
        trigger.note_client_departed(0);
        assert_eq!(trigger.take_pending(), None);
    }

    #[test]
    fn taking_a_pending_action_consumes_it() {
        let mut trigger = drained(ResetPolicy::Reset);
        assert_eq!(trigger.take_pending(), Some(ResetAction::Reset));
        assert_eq!(trigger.take_pending(), None);
    }
}
