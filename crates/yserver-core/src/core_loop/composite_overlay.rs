//! COMPOSITE overlay-window (COW) claim ownership.
//!
//! Implements
//! `docs/superpowers/specs/2026-09-09-composite-overlay-claim-ownership-design.md`.
//!
//! The claim on the overlay is a **per-client** thing, exactly as it is
//! in Xorg, where every `GetOverlayWindow` mallocs a
//! `CompOverlayClientRec` owned by the calling client
//! (`composite/compoverlay.c`) and the resource system frees them all
//! when that client goes away. We keep the equivalent list in
//! [`ServerState::cow_claims`] and the backend counts nothing: it sees
//! only the 0 → 1 materialize edge and the 1 → 0 teardown edge.
//!
//! Two things in the tree carry confusingly similar names and are **not**
//! this: `scene.root_overlay` / `root_overlay_on_disconnect`
//! (`kms/render/backend.rs`), the scene's root-overlay contribution,
//! which `Backend::client_disconnected` already tears down correctly.

use yserver_protocol::x11::ClientId;

use crate::{
    backend::{Backend, OriginContext},
    core_loop::fanout::emit_window_event_to_state,
    resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_WINDOW},
    server::ServerState,
};

const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
const SUBSTRUCTURE_NOTIFY: u32 = 0x0008_0000;

/// Ask the backend to materialize the overlay, then mirror it on the
/// resources side. Called on the 0 → 1 claim edge only.
///
/// The caller must have already recorded the claim, and must roll it
/// back if this returns `Err` — a claim recorded against an overlay that
/// does not exist is exactly the desynchronisation this design removes.
///
/// # Errors
///
/// Whatever the backend's storage allocation reports.
pub(crate) fn materialize_overlay(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
) -> std::io::Result<()> {
    if !backend.get_overlay_window(origin)? {
        // Backends with no COW implementation (v1, ynest, the trait
        // default) never materialize anything, so there is nothing to
        // mirror on the resources side either.
        return Ok(());
    }
    let cow_host_xid = backend
        .cow_host_xid()
        .expect("backend.get_overlay_window returned Ok(true) without populating cow_host_xid");
    state
        .resources
        .materialize_cow_resource(crate::backend::WindowHandle::from_raw_panicking(
            cow_host_xid,
        ));
    // Xorg compCreateOverlayWindow (composite/compoverlay.c:125-141) is a
    // CreateWindow and a MapWindow of an override-redirect root child, so
    // root's SubstructureNotify listeners see CreateNotify then MapNotify.
    // It has no input shape: it takes the pointer until the compositor
    // empties its input region.
    if let Some(geometry) =
        state
            .resources
            .window(COMPOSITE_OVERLAY_WINDOW)
            .map(|w| yserver_protocol::x11::Geometry {
                root: ROOT_WINDOW,
                x: w.x,
                y: w.y,
                width: w.width,
                height: w.height,
                border_width: w.border_width,
                depth: w.depth,
            })
    {
        let _dropped =
            emit_window_event_to_state(state, ROOT_WINDOW, SUBSTRUCTURE_NOTIFY, |buf, seq, o| {
                yserver_protocol::x11::encode_create_notify_event(
                    buf,
                    seq,
                    o,
                    ROOT_WINDOW,
                    COMPOSITE_OVERLAY_WINDOW,
                    geometry,
                    true,
                );
            });
        let _dropped =
            emit_window_event_to_state(state, ROOT_WINDOW, SUBSTRUCTURE_NOTIFY, |buf, seq, o| {
                yserver_protocol::x11::encode_map_notify_event(
                    buf,
                    seq,
                    o,
                    ROOT_WINDOW,
                    COMPOSITE_OVERLAY_WINDOW,
                    true,
                );
            });
    }
    // The COW is now a core root child (capped on top); reproject the
    // backend top-level order from core so it enters the projection at
    // the top. Must run AFTER materialize_cow_resource — the backend COW
    // hook no longer pushes to top_level_order itself.
    backend.sync_top_level_order(state);
    // The MapWindow ends in WindowsRestructured (`dix/window.c:2695`).
    backend.windows_restructured(state);
    Ok(())
}

/// Tear the overlay down: backend first, then the resources-side mirror.
///
/// Called on the 1 → 0 claim edge only. Backend first is load-bearing:
/// the claim is the thing keeping the overlay alive, so a caller must not
/// drop the last claim unless this succeeded.
///
/// # Errors
///
/// From the backend's teardown — on `KmsBackend` the
/// `materialize_direct_shadow_for_unflip` allocation that has to succeed
/// before a scanned-out buffer can be released.
pub(crate) fn teardown_overlay(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
) -> std::io::Result<()> {
    let cow_host_xid = backend.cow_host_xid();
    if !backend.release_overlay_window(origin)? {
        // Backend never materialized a COW (v1, ynest, trait default):
        // nothing to mirror down either, because nothing ever reached
        // `materialize_cow_resource`.
        return Ok(());
    }
    // Xorg frees the overlay through DeleteWindow (dix/window.c:1070): an
    // UnmapNotify then a DestroyNotify, each to the COW's StructureNotify
    // and root's SubstructureNotify listeners.
    let cow_mapped = state
        .resources
        .window(COMPOSITE_OVERLAY_WINDOW)
        .is_some_and(|w| w.map_state != crate::resources::MapState::Unmapped);
    let targets = [
        (COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY),
        (ROOT_WINDOW, SUBSTRUCTURE_NOTIFY),
    ];
    if cow_mapped {
        for (event_window, mask) in targets {
            let _dropped = emit_window_event_to_state(state, event_window, mask, |buf, seq, o| {
                yserver_protocol::x11::encode_unmap_notify_event(
                    buf,
                    seq,
                    o,
                    event_window,
                    COMPOSITE_OVERLAY_WINDOW,
                    false,
                );
            });
        }
        // DeleteWindow's UnmapWindow re-evaluates the pointer while the
        // overlay still exists (`dix/window.c:2871`).
        let _ = state.resources.unmap_window(COMPOSITE_OVERLAY_WINDOW);
        backend.windows_restructured(state);
    }
    for (event_window, mask) in targets {
        let _dropped = emit_window_event_to_state(state, event_window, mask, |buf, seq, o| {
            yserver_protocol::x11::encode_destroy_notify_event(
                buf,
                seq,
                o,
                event_window,
                COMPOSITE_OVERLAY_WINDOW,
            );
        });
    }
    // Xorg frees the overlay through DeleteWindow, so its Pictures die with it.
    crate::core_loop::process_request::free_pictures_on_destroyed_windows(
        state,
        backend,
        origin,
        &[COMPOSITE_OVERLAY_WINDOW],
    );
    crate::core_loop::process_request::purge_present_for_destroyed_windows(
        state,
        backend,
        &[COMPOSITE_OVERLAY_WINDOW],
    );
    state.resources.destroy_cow_resource();
    // Unregistered only now: the unmap's crossings above still name the COW.
    if let Some(xid) = cow_host_xid {
        backend.unregister_host_window(xid);
    }
    state.destroy_cow_input_shape();
    // The COW is no longer a core root child; reproject so it leaves the
    // backend top-level order.
    backend.sync_top_level_order(state);
    Ok(())
}

/// Release **every** overlay claim held by `client`, and tear the overlay
/// down if that was the last one anywhere.
///
/// Called from `process_disconnect` itself — deliberately not from
/// `disconnect_with_pending_cleanup`: `KillClient` on another client's
/// resource calls `process_disconnect` inline and bypasses that funnel,
/// so a helper placed in the funnel would miss the killed compositor.
///
/// **Claims are released whatever the close-down mode**, unlike ordinary
/// resources. `RetainPermanent` / `RetainTemporary` keep pixmaps, GCs and
/// fonts alive for a later client to adopt; the overlay is not that kind
/// of thing. It is a screen-wide singleton, so a zombie holding it would
/// block every future compositor for the life of the server, with no
/// client left to ask for it back. This is a deliberate divergence from
/// Xorg, whose `FakeClientID` overlay records follow the ordinary retain
/// rules; retention semantics for a screen-wide singleton are not worth
/// inheriting.
///
/// If the final teardown fails the claims are **still** released — no
/// claim outlives its owner, and a leftover would be indistinguishable
/// from a live one, leaving the next compositor waiting on a client that
/// no longer exists — **and** the server additionally enters
/// [`ServerState::cow_teardown_failed`]. The two are not alternatives:
/// the claims go, and that state owns the orphaned overlay afterwards.
pub(crate) fn release_client_overlay_claims(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client: ClientId,
) {
    let before = state.cow_claims.len();
    state.cow_claims.retain(|owner| *owner != client);
    let released = before - state.cow_claims.len();
    if released == 0 {
        return;
    }
    log::debug!(
        "client {} departed holding {released} COMPOSITE overlay claim(s); {} remain",
        client.0,
        state.cow_claims.len(),
    );
    if !state.cow_claims.is_empty() {
        return;
    }
    if let Err(err) = teardown_overlay(state, backend, None) {
        // The claimant is gone and cannot retry, so there is no
        // "retryable" state to be in. Record the session-fatal condition
        // instead: the overlay stays materialized and unclaimable until
        // the process ends.
        log::error!(
            "client {} departed as the last overlay claimant but the COW \
             teardown failed: {err}. Entering cow_teardown_failed — no new \
             compositor may take the overlay in this session.",
            client.0,
        );
        state.cow_teardown_failed = true;
    }
}
