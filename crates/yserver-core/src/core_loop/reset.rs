//! Forced session teardown for the server-reset generation boundary
//! (`docs/superpowers/specs/2026-09-09-server-reset-design.md`, "Forced
//! cleanup, not the normal disconnect path").
//!
//! Nothing in production calls this yet — the reset boundary that will
//! (`reset_generation`) is a later step. It exists on its own so it can be
//! proved on its own.

use yserver_protocol::x11::ClientId;

use crate::{
    backend::{Backend, PixmapHandle},
    core_loop::process_disconnect::{
        HostPixmapFrees, destroy_zombie_resources_reporting, process_disconnect_reporting,
    },
    server::ServerState,
};

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

    use super::force_destroy_all_clients;
    use crate::{
        backend::{Backend, PixmapHandle, WindowHandle, recording::RecordingBackend},
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
}
