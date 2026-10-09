use super::*;

/// Manual-mode redirect removes W from the normal scene and keeps
/// B as an off-screen compositor source. B must not become a scene
/// participant, or the scene path can double-present redirected
/// client content behind/above the compositor's output surface.
#[test]
fn manual_redirect_keeps_window_out_of_scene() {
    use crate::backend::recording::RecordedCall;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Install a top-level child of root (the soon-to-be-Manual-
    // redirected window). Use real CreateWindow flow so the
    // resource record carries width/height/depth that
    // `activate_redirect_backing_for` snapshots.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    // Viewable: Xorg allocates a redirect backing only for a realized window.
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    state
        .composite_redirects
        .redirect_window(
            ResourceId(WINDOW_XID),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: yserver_protocol::x11::ClientId(1),
            },
        )
        .unwrap();

    activate_redirect_backing_for(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        crate::server::CompositeRedirectMode::Manual,
    );

    // Window participation: Manual → false (the external
    // compositor owns the window's presentation). The paired
    // assertion that B *is* scene-participating lives in
    // `manual_redirect_marks_backing_scene_participating_so_paints_emit_damage`.
    let calls = backend.calls();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetWindowSceneParticipation {
                host_window,
                participating: false,
            } if *host_window == HOST_XID
        )),
        "expected SetWindowSceneParticipation(W, false) for Manual mode; got {calls:?}",
    );
    // Positive control for the COW guard
    // (`activate_redirect_on_cow_is_never_applied`): a NORMAL window
    // DOES receive a redirect backing. This proves the COW guard is
    // overlay-window-specific, not a blanket disable of
    // `activate_redirect_backing_for`.
    assert!(
        state
            .resources
            .window(ResourceId(WINDOW_XID))
            .expect("window exists")
            .redirected_backing
            .is_some(),
        "a normal window must receive a redirect backing",
    );
}

/// Xorg `compCheckRedirect` (`composite/compwindow.c:156-170`) forces
/// `should = FALSE` for `pWin == cs->pOverlayWin`: the overlay window
/// is NEVER actually redirected, regardless of trigger. Since Phase 2
/// made the COW a real child of root, a compositor's
/// `CompositeRedirectSubwindows(root, Manual)` (mutter / cinnamon-mutter
/// all issue it) drives `activate_redirect_backing_for` once per root
/// child — including the COW. Without the guard the COW gets a Manual
/// backing + `scene_participating=false`, and the Phase 3 Manual-skip
/// drops the whole composited desktop from scanout (blank desktop,
/// observed bee/cinnamon-mutter 2026-06-09). The COW must keep its
/// `redirected_backing` empty and its scene participation untouched.
#[test]
fn activate_redirect_on_cow_is_never_applied() {
    use crate::backend::recording::RecordedCall;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Materialize the COW exactly as the `GetOverlayWindow` 0->1
    // claim does (host xid == COMPOSITE_OVERLAY_WINDOW.0 for the
    // RecordingBackend / v2 path, per the dispatch handler).
    let cow_host = crate::backend::WindowHandle::from_raw_panicking(COMPOSITE_OVERLAY_WINDOW.0);
    state.resources.materialize_cow_resource(cow_host);

    // This is the call `RedirectSubwindows(root, Manual)`'s child
    // loop makes for every child of root — the COW included.
    activate_redirect_backing_for(
        &mut state,
        &mut backend,
        None,
        COMPOSITE_OVERLAY_WINDOW,
        crate::server::CompositeRedirectMode::Manual,
    );

    let cow = state
        .resources
        .window(COMPOSITE_OVERLAY_WINDOW)
        .expect("COW exists");
    assert!(
        cow.redirected_backing.is_none(),
        "COW must never receive a redirect backing \
             (compCheckRedirect: should=FALSE for pOverlayWin)",
    );
    // And no scene-participation flip was recorded for the COW host —
    // the Phase 3 Manual-skip is driven off this flag.
    let calls = backend.calls();
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetWindowSceneParticipation { host_window, .. }
                if *host_window == COMPOSITE_OVERLAY_WINDOW.0
        )),
        "COW scene participation must not be flipped by redirect activation; got {calls:?}",
    );
}

/// Manual-redirected backing must have scene_participating=true so
/// the v2 scene's damage harvest sees paints into it.
///
/// Post-`6ffd370` the scene walk emits the Manual-redirected window
/// at its own coords but samples storage from the backing via
/// `redirected_target` indirection. `store.damage()` is gated on
/// the *target's* `scene_participating` — if the backing is marked
/// non-participating, every paint into it silently drops its scene
/// damage. Buffer-age clipped compose then never repaints the
/// affected region, and the BO retains whatever was in it the last
/// time it was flipped. Symptom: window content invisible except
/// where cursor damage happens to overlap the window's rect.
#[test]
fn manual_redirect_marks_backing_scene_participating_so_paints_emit_damage() {
    use crate::backend::recording::RecordedCall;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    // Viewable: Xorg allocates a redirect backing only for a realized window.
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    state
        .composite_redirects
        .redirect_window(
            ResourceId(WINDOW_XID),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: yserver_protocol::x11::ClientId(1),
            },
        )
        .unwrap();

    activate_redirect_backing_for(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        crate::server::CompositeRedirectMode::Manual,
    );

    let calls = backend.calls();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetBackingSceneParticipation {
                participating: true,
                ..
            }
        )),
        "expected SetBackingSceneParticipation(B, true) for Manual mode so the \
             scene's redirected_target damage-peek sees paints; got {calls:?}",
    );
}

/// Redirecting an already-mapped window must emit one initial full
/// DamageNotify wakeup so a compositor that subscribes after map
/// can pull the seeded backing into the COW immediately. Without
/// this, the backing contains the correct pixels but the COW keeps
/// showing the root/background until some unrelated later event
/// (click, move, resize) dirties the window.
#[test]
fn activate_redirect_on_mapped_window_emits_initial_damage() {
    use crate::server::{DamageObject, RedirectRecord};

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0010;
    const HOST_XID: u32 = 0x0040_0010;
    const DAMAGE_XID: u32 = 0x0010_0011;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(WINDOW_XID));
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    state
        .composite_redirects
        .redirect_window(
            ResourceId(WINDOW_XID),
            RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: yserver_protocol::x11::ClientId(CLIENT_ID),
            },
        )
        .unwrap();
    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(CLIENT_ID),
            drawable: ResourceId(WINDOW_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    activate_redirect_backing_for(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        crate::server::CompositeRedirectMode::Manual,
    );

    let damage = state
        .damage_objects
        .get(&DAMAGE_XID)
        .expect("damage object after redirect activation");
    assert!(
        !damage.rects.is_empty(),
        "redirect activation on an already-mapped window must emit an initial \
             full damage wakeup so compositors pull the seeded backing immediately",
    );
}

/// A compositor subscribes to DAMAGE on the `NameWindowPixmap`
/// pixmap, not the original window. That pixmap must receive the
/// same initial seed as a plain viewable window, otherwise the
/// first composite after redirect can stay blank until some later
/// client paint lands.
#[test]
fn damage_create_on_named_pixmap_alias_seeds_initial_damage() {
    use yserver_protocol::x11::damage as x11damage;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0012;
    const HOST_WINDOW_XID: u32 = 0x0040_0012;
    const PIXMAP_XID: u32 = 0x0020_0012;
    const DAMAGE_ID: u32 = 0x0080_0012;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            HOST_WINDOW_XID,
        ));
        w.map_state = crate::resources::MapState::Viewable;
    }
    activate_redirect_backing_for(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        crate::server::CompositeRedirectMode::Manual,
    );
    let host_pixmap = state
        .resources
        .window(ResourceId(WINDOW_XID))
        .and_then(|w| w.redirected_backing.as_ref())
        .map(|b| b.host_pixmap)
        .expect("redirected backing");
    state.resources.create_pixmap(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreatePixmapRequest {
            pixmap: ResourceId(PIXMAP_XID),
            drawable: ResourceId(WINDOW_XID),
            width: 100,
            height: 50,
            depth: 24,
        },
    );
    let _ = state
        .resources
        .set_pixmap_host_xid(ResourceId(PIXMAP_XID), host_pixmap);
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.composite_named_pixmaps
            .push(crate::resources::NamedCompositePixmap {
                client_pixmap: ResourceId(PIXMAP_XID),
                host_pixmap,
                width: 100,
                height: 50,
            });
    }

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&DAMAGE_ID.to_le_bytes());
    body.extend_from_slice(&PIXMAP_XID.to_le_bytes());
    body.push(x11damage::report_level::NON_EMPTY);
    body.extend_from_slice(&[0, 0, 0]);

    let header = RequestHeader {
        opcode: 0,
        data: x11damage::CREATE,
        length_units: 0,
    };
    handle_damage_request(
        &mut state,
        &mut backend,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("damage create handled");

    let damage = state
        .damage_objects
        .get(&DAMAGE_ID)
        .expect("damage object created");
    assert!(
        !damage.rects.is_empty(),
        "creating DAMAGE on a named pixmap alias of a viewable window must seed \
             initial damage so picom sees the current backing immediately",
    );
}

/// Mapping a Manual-redirected window must not leave it visible
/// in the normal scene. The map path temporarily flips the
/// storage on, then the redirect-mode reapply must force it back
/// off.
#[test]
fn map_window_reapplies_manual_redirect_scene_participation() {
    use crate::backend::recording::RecordedCall;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0002;
    const HOST_XID: u32 = 0x0040_0002;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    state
        .composite_redirects
        .redirect_window(
            ResourceId(WINDOW_XID),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: yserver_protocol::x11::ClientId(CLIENT_ID),
            },
        )
        .unwrap();

    activate_redirect_backing_for(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        crate::server::CompositeRedirectMode::Manual,
    );
    let before = backend.calls().len();

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window");

    let calls = &backend.calls()[before..];
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetWindowSceneParticipation {
                host_window,
                participating: false,
            } if *host_window == HOST_XID
        )),
        "mapping a Manual-redirected window must reassert scene_participating=false; got {calls:?}",
    );
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetWindowSceneParticipation {
                host_window,
                participating: true,
            } if *host_window == HOST_XID
        )),
        "mapping a Manual-redirected window must not leave it scene_participating=true; got {calls:?}",
    );
}

/// Audit #11 regression — when a window maps, server-background
/// paint fills the window's storage but no `DamageNotify` fires
/// for the affected region. Compositors that subscribe to
/// `XDamageCreate(window)` (marco-with-compositing, picom) miss
/// the first frame and the window stays invisible on COW until
/// the next paint into it. Symptom: override-redirect popup
/// menus, mate-panel system-tray icons (`nm-applet`) don't show
/// when a compositor is active and v2 routes the visible output
/// through `Present::Pixmap → COW`.
///
/// Oracle: a `DamageObject` subscribed to a freshly-created
/// window must accumulate damage rects covering the window's
/// full extent after `handle_map_window` returns.
#[test]
fn map_window_emits_damage_on_window_extent() {
    use crate::server::DamageObject;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0010;
    const HOST_XID: u32 = 0x0040_0010;
    const DAMAGE_ID: u32 = 0x0080_0010;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }

    // Compositor subscribes via `XDamageCreate(window)`. Pin the
    // pre-state explicitly: no rects accumulated yet.
    state.damage_objects.insert(
        DAMAGE_ID,
        DamageObject {
            owner: ClientId(CLIENT_ID),
            drawable: ResourceId(WINDOW_XID),
            level: 0,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window");

    let damage = state
        .damage_objects
        .get(&DAMAGE_ID)
        .expect("damage_object survived map");
    assert!(
        !damage.rects.is_empty(),
        "MapWindow must fire damage on the window's extent so a \
             subscribed compositor repaints the new region into its \
             offscreen — pre-fix this is empty and the compositor \
             never sees the newly-mapped popup / tray icon / window",
    );
}

#[test]
fn map_window_seeds_damage_for_newly_viewable_descendant() {
    use crate::server::DamageObject;

    const CLIENT_ID: u32 = 1;
    const PARENT_XID: u32 = 0x0010_0012;
    const PARENT_HOST_XID: u32 = 0x0040_0012;
    const CHILD_XID: u32 = 0x0010_0013;
    const CHILD_HOST_XID: u32 = 0x0040_0013;
    const CHILD_DAMAGE_ID: u32 = 0x0080_0013;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(PARENT_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 400,
            height: 300,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_XID),
            parent: ResourceId(PARENT_XID),
            x: 20,
            y: 30,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let parent = state
            .resources
            .window_mut(ResourceId(PARENT_XID))
            .expect("parent installed");
        parent.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            PARENT_HOST_XID,
        ));
        parent.map_state = crate::resources::MapState::Unmapped;
    }
    {
        let child = state
            .resources
            .window_mut(ResourceId(CHILD_XID))
            .expect("child installed");
        child.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            CHILD_HOST_XID,
        ));
        child.map_state = crate::resources::MapState::Unviewable;
    }

    state.damage_objects.insert(
        CHILD_DAMAGE_ID,
        DamageObject {
            owner: ClientId(CLIENT_ID),
            drawable: ResourceId(CHILD_XID),
            level: 0,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&PARENT_XID.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window");

    let child_damage = state
        .damage_objects
        .get(&CHILD_DAMAGE_ID)
        .expect("child damage object survives");
    assert!(
        !child_damage.rects.is_empty(),
        "mapping a parent must seed damage for descendants promoted from \
             Unviewable to Viewable; otherwise compositors miss the child's \
             first visible frame"
    );
}

#[test]
fn map_window_rereports_nonempty_damage_for_newly_viewable_descendant() {
    use crate::{nested::DAMAGE_FIRST_EVENT, server::DamageObject};
    use std::io::Read;
    use yserver_protocol::x11::{damage as x11damage, xfixes::RegionRect};

    const CLIENT_ID: u32 = 1;
    const PARENT_XID: u32 = 0x0010_0022;
    const PARENT_HOST_XID: u32 = 0x0040_0022;
    const CHILD_XID: u32 = 0x0010_0023;
    const CHILD_HOST_XID: u32 = 0x0040_0023;
    const CHILD_DAMAGE_ID: u32 = 0x0080_0023;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(PARENT_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 400,
            height: 300,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_XID),
            parent: ResourceId(PARENT_XID),
            x: 20,
            y: 30,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let parent = state
            .resources
            .window_mut(ResourceId(PARENT_XID))
            .expect("parent installed");
        parent.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            PARENT_HOST_XID,
        ));
        parent.map_state = crate::resources::MapState::Unmapped;
    }
    {
        let child = state
            .resources
            .window_mut(ResourceId(CHILD_XID))
            .expect("child installed");
        child.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            CHILD_HOST_XID,
        ));
        child.map_state = crate::resources::MapState::Unviewable;
    }

    state.damage_objects.insert(
        CHILD_DAMAGE_ID,
        DamageObject {
            owner: ClientId(CLIENT_ID),
            drawable: ResourceId(CHILD_XID),
            level: x11damage::report_level::NON_EMPTY,
            rects: vec![RegionRect {
                x: 0,
                y: 0,
                width: 200,
                height: 80,
            }],
            pending_notify_fired: true,
            last_reported_geometry: Some(x11damage::Rectangle {
                x: 20,
                y: 30,
                width: 200,
                height: 80,
            }),
        },
    );

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&PARENT_XID.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window");

    peer.set_nonblocking(true).expect("set nonblocking");
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    assert!(
        all.chunks_exact(32).any(|evt| evt[0] == DAMAGE_FIRST_EVENT),
        "mapping a parent must re-report NON_EMPTY damage for descendants promoted \
             from Unviewable to Viewable even when they already fired before becoming viewable"
    );
}

#[test]
fn damage_subtract_flushes_coalesced_paint_before_consuming_damage() {
    use yserver_protocol::x11::{damage, xfixes::RegionRect};

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let rects = vec![RegionRect {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    }];
    // NonEmpty damage has already notified the compositor; later paints
    // accumulate without another notification/submission boundary.
    state.damage_objects.insert(
        0x100,
        crate::server::DamageObject {
            owner: ClientId(1),
            drawable: ROOT_WINDOW,
            level: damage::report_level::NON_EMPTY,
            rects: rects.clone(),
            pending_notify_fired: true,
            last_reported_geometry: None,
        },
    );
    let body: Vec<u8> = [0x100_u32, 0, 0x200]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 143,
            data: damage::SUBTRACT,
            length_units: 4,
        },
        &body,
        None,
    )
    .expect("Subtract");

    assert_eq!(state.xfixes_regions[&0x200].rects, rects);
    assert!(state.damage_objects[&0x100].rects.is_empty());
    assert_eq!(
        backend.calls(),
        vec![RecordedCall::FlushBeforeDamageNotify],
        "Subtract must submit paint even when there is no new DamageNotify"
    );
}

#[test]
fn fetch_region_flushes_paint_before_exposing_damage_rectangles() {
    use yserver_protocol::x11::xfixes;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.xfixes_client_major.insert(1, 5);
    let mut backend = RecordingBackend::new();
    state.xfixes_regions.insert(
        0x200,
        crate::server::XFixesRegion {
            owner: ClientId(1),
            rects: vec![xfixes::RegionRect {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            }],
        },
    );
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: xfixes::FETCH_REGION,
            length_units: 2,
        },
        &0x200_u32.to_le_bytes(),
        None,
    )
    .expect("FetchRegion");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 40);
    assert_eq!(bytes[0], 1, "reply, not an error");
    assert_eq!(&bytes[32..], &[10, 0, 20, 0, 30, 0, 40, 0]);
    assert_eq!(
        backend.calls(),
        vec![RecordedCall::FlushBeforeDamageNotify],
        "FetchRegion must submit paint before publishing its damage region"
    );
}

#[test]
fn damage_create_on_viewable_window_seeds_full_damage() {
    use yserver_protocol::x11::{RequestHeader, damage as x11damage};

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0100;
    const HOST_XID: u32 = 0x0040_0100;
    const DAMAGE_ID: u32 = 0x0080_0100;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);

    state.resources.create_window(
        ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 11,
            y: 22,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
        w.map_state = crate::resources::MapState::Viewable;
    }
    let mut body = Vec::new();
    body.extend_from_slice(&DAMAGE_ID.to_le_bytes());
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    body.push(x11damage::report_level::NON_EMPTY);
    body.extend_from_slice(&[0, 0, 0]);

    let header = RequestHeader {
        opcode: 0,
        data: x11damage::CREATE,
        length_units: 0,
    };
    handle_damage_request(
        &mut state,
        &mut RecordingBackend::new(),
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("damage create handled");

    let damage = state
        .damage_objects
        .get(&DAMAGE_ID)
        .expect("damage object created");
    assert!(
        !damage.rects.is_empty(),
        "creating DAMAGE on an already-viewable window must seed initial \
             damage so the subscriber does not miss the current visible frame"
    );
}

fn dispatch_damage_subtract(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    damage_id: u32,
    repair: u32,
    parts: u32,
) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&damage_id.to_le_bytes());
    body.extend_from_slice(&repair.to_le_bytes());
    body.extend_from_slice(&parts.to_le_bytes());
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 143, // DAMAGE major
            data: yserver_protocol::x11::damage::SUBTRACT,
            length_units: 4,
        },
        &body,
        None,
    )
    .unwrap();
}

#[test]
fn damage_subtract_with_no_repair_returns_old_damage_in_parts_and_clears() {
    use crate::server::{DamageObject, XFixesRegion};
    use yserver_protocol::x11::xfixes::RegionRect;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let damaged_rect = RegionRect {
        x: 10,
        y: 20,
        width: 30,
        height: 40,
    };
    state.damage_objects.insert(
        0x20,
        DamageObject {
            owner: ClientId(1),
            drawable: ResourceId(0x100),
            level: 0,
            rects: vec![damaged_rect],
            pending_notify_fired: true,
            last_reported_geometry: None,
        },
    );
    // Pre-create the parts region so the handler writes into it.
    state.xfixes_regions.insert(
        0x21,
        XFixesRegion {
            owner: ClientId(1),
            rects: Vec::new(),
        },
    );

    dispatch_damage_subtract(&mut state, &mut backend, 0x20, 0, 0x21);

    // Per X11 DAMAGE spec: repair==None → parts gets old damage,
    // damage is cleared.
    let parts = state
        .xfixes_regions
        .get(&0x21)
        .expect("parts region must exist");
    assert_eq!(
        parts.rects,
        vec![damaged_rect],
        "with repair=None, parts must receive the entire old damage region; \
             pre-fix this returns Vec::new() and starves compositors of a real clip"
    );
    let damage = state.damage_objects.get(&0x20).unwrap();
    assert!(
        damage.rects.is_empty(),
        "with repair=None, damage must be fully cleared"
    );
    assert!(
        !damage.pending_notify_fired,
        "Subtract is the cycle boundary — pending_notify_fired must reset"
    );
}

#[test]
fn damage_subtract_with_no_repair_canonicalizes_parts_region() {
    use crate::server::{DamageObject, XFixesRegion};
    use yserver_protocol::x11::xfixes::RegionRect;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    state.damage_objects.insert(
        0x40,
        DamageObject {
            owner: ClientId(1),
            drawable: ResourceId(0x100),
            level: 0,
            rects: vec![
                RegionRect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                RegionRect {
                    x: 10,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                RegionRect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 10,
                },
            ],
            pending_notify_fired: true,
            last_reported_geometry: None,
        },
    );
    state.xfixes_regions.insert(
        0x41,
        XFixesRegion {
            owner: ClientId(1),
            rects: Vec::new(),
        },
    );

    dispatch_damage_subtract(&mut state, &mut backend, 0x40, 0, 0x41);

    let parts = state
        .xfixes_regions
        .get(&0x41)
        .expect("parts region must exist");
    assert_eq!(
        parts.rects,
        vec![RegionRect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        }],
        "with repair=None, parts must be canonicalized before they are handed to the client"
    );
}

#[test]
fn damage_subtract_with_repair_returns_intersection_and_subtracts_from_damage() {
    use crate::server::{DamageObject, XFixesRegion};
    use yserver_protocol::x11::xfixes::RegionRect;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // Damage: 100x100 at origin.
    state.damage_objects.insert(
        0x30,
        DamageObject {
            owner: ClientId(1),
            drawable: ResourceId(0x100),
            level: 0,
            rects: vec![RegionRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }],
            pending_notify_fired: true,
            last_reported_geometry: None,
        },
    );
    // Repair: right-half strip 50x100 at (50, 0). MUST remain
    // unchanged by Subtract — it's an input filter, not an output.
    let repair_rect = RegionRect {
        x: 50,
        y: 0,
        width: 50,
        height: 100,
    };
    state.xfixes_regions.insert(
        0x31,
        XFixesRegion {
            owner: ClientId(1),
            rects: vec![repair_rect],
        },
    );
    state.xfixes_regions.insert(
        0x32,
        XFixesRegion {
            owner: ClientId(1),
            rects: Vec::new(),
        },
    );

    dispatch_damage_subtract(&mut state, &mut backend, 0x30, 0x31, 0x32);

    // parts = damage ∩ repair  →  right half 50x100 at (50, 0).
    let parts = &state.xfixes_regions.get(&0x32).unwrap().rects;
    let parts_area: u64 = parts
        .iter()
        .map(|r| u64::from(r.width) * u64::from(r.height))
        .sum();
    assert_eq!(
        parts_area,
        50 * 100,
        "parts must cover the intersection (5000 px²); got {parts:?}"
    );
    for r in parts {
        assert!(
            r.x >= 50,
            "parts rect {r:?} extends outside the repair filter"
        );
    }

    // damage = damage − repair  →  left half 50x100 at (0, 0).
    let damage = &state.damage_objects.get(&0x30).unwrap().rects;
    let damage_area: u64 = damage
        .iter()
        .map(|r| u64::from(r.width) * u64::from(r.height))
        .sum();
    assert_eq!(
        damage_area,
        50 * 100,
        "damage must retain the un-repaired left half (5000 px²); got {damage:?}"
    );
    for r in damage {
        assert!(
            r.x + i16::try_from(r.width).unwrap_or(i16::MAX) <= 50,
            "damage rect {r:?} extends into the repaired region"
        );
    }

    // repair region MUST NOT have been overwritten — pre-fix this
    // stomped repair with `damage.rects.clone()`.
    let repair_now = &state.xfixes_regions.get(&0x31).unwrap().rects;
    assert_eq!(
        repair_now,
        &vec![repair_rect],
        "Subtract must treat the repair region as read-only input; \
             pre-fix this overwrote it with the damage rects"
    );
}

#[test]
fn damage_subtract_with_remaining_nonempty_damage_rereports_immediately() {
    use crate::server::{DamageObject, XFixesRegion};
    use yserver_protocol::x11::{CreateWindowRequest, damage as x11damage, xfixes::RegionRect};

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _peer = install_client(&mut state, 1);

    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x100),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(0x100));

    state.damage_objects.insert(
        0x40,
        DamageObject {
            owner: ClientId(1),
            drawable: ResourceId(0x100),
            level: x11damage::report_level::NON_EMPTY,
            rects: vec![RegionRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }],
            pending_notify_fired: true,
            last_reported_geometry: None,
        },
    );
    state.xfixes_regions.insert(
        0x41,
        XFixesRegion {
            owner: ClientId(1),
            rects: vec![RegionRect {
                x: 50,
                y: 0,
                width: 50,
                height: 100,
            }],
        },
    );
    state.xfixes_regions.insert(
        0x42,
        XFixesRegion {
            owner: ClientId(1),
            rects: Vec::new(),
        },
    );

    dispatch_damage_subtract(&mut state, &mut backend, 0x40, 0x41, 0x42);

    let damage = state.damage_objects.get(&0x40).expect("damage object");
    assert!(
        !damage.rects.is_empty(),
        "repair only covered part of the damage; some damage must remain"
    );
    assert!(
        damage.pending_notify_fired,
        "Xorg immediately re-reports remaining non-empty damage after Subtract; \
             pre-fix yserver left fired=false and waited for unrelated future drawing"
    );
}

/// X11 Composite + DAMAGE interaction: a configure that changes a
/// redirected window's screen-space presentation must emit a
/// `DamageNotify` to the compositor's damage subscription on that
/// window. Xorg behaviour (compositor.c) — without this event,
/// marco/picom/etc. never mark the moved window dirty and their
/// SetPictureClipRectangles excludes the window's region, so
/// composites against the redirected backing no-op or partially
/// blend, producing the "CC disappears on drag / muddy bands on
/// caja-redraw" symptom we measured against a Xephyr (Xorg-family)
/// reference run: yserver emitted 0 DamageNotify events to marco;
/// Xephyr emitted 776.
///
/// Reproduce minimally: install marco client, create a top-level
/// window with a damage object owned by marco, set the window
/// `redirected_backing` (Manual-redirect activated state), then
/// send a move-only ConfigureWindow. Read marco's socket and
/// look for the DamageNotify event (event opcode 94 = DAMAGE
/// first_event + 0).
#[test]
fn configure_window_on_redirected_window_emits_damage_to_subscriber() {
    use crate::{
        resources::{ROOT_WINDOW, RedirectedBacking},
        server::{CompositeRedirectMode, DamageObject, RedirectRecord},
    };
    use std::io::Read;
    use yserver_protocol::x11::CreateWindowRequest;
    const MARCO: u32 = 14;
    const FRAME_XID: u32 = 0x0010_0001;
    const DAMAGE_XID: u32 = 0x0010_0002;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, MARCO);

    // Frame W under root.
    state.resources.create_window(
        ClientId(MARCO),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(FRAME_XID),
            parent: ROOT_WINDOW,
            x: 100,
            y: 100,
            width: 997,
            height: 652,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(FRAME_XID));
    state
        .composite_redirects
        .redirect_window(
            ResourceId(FRAME_XID),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(MARCO),
            },
        )
        .unwrap();
    // Mark the frame as redirected (Manual-redirect activated
    // state). `host_pixmap` value is opaque to the damage path.
    if let Some(w) = state.resources.window_mut(ResourceId(FRAME_XID)) {
        w.host_xid = crate::backend::WindowHandle::from_raw(0xDEAD_BEEF);
        w.redirected_backing = Some(RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw(0xC0DE).unwrap(),
            width: 997,
            height: 652,
            depth: 24,
        });
    }
    // Damage subscription on the frame, owned by marco, NonEmpty
    // level (0x03) so any change fires.
    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(MARCO),
            drawable: ResourceId(FRAME_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    // ConfigureWindow body: window(4) + value_mask(2) + pad(2) +
    // x(4 as i16) + y(4 as i16). value_mask = 0x03 = CWX | CWY.
    // Move from (100, 100) to (250, 300).
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&FRAME_XID.to_le_bytes());
    body.extend_from_slice(&0x0003u16.to_le_bytes()); // value_mask
    body.extend_from_slice(&[0u8; 2]); // pad
    body.extend_from_slice(&250i32.to_le_bytes()); // x
    body.extend_from_slice(&300i32.to_le_bytes()); // y
    process_request(
        &mut state,
        &mut backend,
        ClientId(MARCO),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12, // ConfigureWindow
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    // Drain marco's socket. Look for a DamageNotify event:
    // event_code = DAMAGE_FIRST_EVENT + 0.
    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let damage_evt: [u8; 32] = all
        .chunks_exact(32)
        .find(|evt| evt[0] == crate::nested::DAMAGE_FIRST_EVENT)
        .map(|s| {
            let mut a = [0u8; 32];
            a.copy_from_slice(s);
            a
        })
        .unwrap_or_else(|| {
            panic!(
                "ConfigureWindow on a redirected window must emit DamageNotify \
                     (event code 94) to the compositor's damage subscriber. \
                     Pre-fix yserver emits 0 DamageNotify events vs Xephyr's 776, \
                     which leaves marco's compositor unable to mark the moved \
                     window dirty. Got {} bytes; first 32: {:02x?}",
                all.len(),
                all.get(..32.min(all.len())).unwrap_or(&[]),
            )
        });
    // Per X11 DAMAGE proto: DamageNotify.geometry encodes the
    // damaged drawable's CURRENT root-relative position + extent
    // (Xorg damageext.c fills from pDrawable->{x,y,width,height};
    // for windows, x/y are root-relative). marco/picom etc. use
    // this to map the damage region into screen space. yserver's
    // encoder historically hardcoded {x: 0, y: 0, w, h}, leaving
    // marco mapping the damage to the wrong screen rect after
    // every move — visible as "top-left bits stay rendered"
    // after dragging (marco recomposites at the OLD position
    // because the geometry origin points back to (0,0)).
    //
    // Wire layout: offsets 24..32 hold geometry as
    // (x i16, y i16, w u16, h u16) little-endian. The window
    // was configured to (250, 300) above.
    let geom_x = i16::from_le_bytes([damage_evt[24], damage_evt[25]]);
    let geom_y = i16::from_le_bytes([damage_evt[26], damage_evt[27]]);
    let geom_w = u16::from_le_bytes([damage_evt[28], damage_evt[29]]);
    let geom_h = u16::from_le_bytes([damage_evt[30], damage_evt[31]]);
    assert_eq!(
        (geom_x, geom_y, geom_w, geom_h),
        (250, 300, 997, 652),
        "DamageNotify.geometry must encode the window's CURRENT \
             root-relative position + extent; pre-fix this hardcodes \
             (0, 0, w, h) and breaks marco's screen-rect mapping",
    );
}

/// Effective redirect state includes inherited `RedirectSubwindows`
/// mode, not just a directly populated `window.redirected_backing`.
/// A move-only ConfigureWindow under an inherited redirect still
/// needs to wake the compositor with DamageNotify.
#[test]
fn configure_window_on_inherited_redirect_emits_damage_to_subscriber() {
    use crate::{
        resources::ROOT_WINDOW,
        server::{CompositeRedirectMode, DamageObject, RedirectRecord},
    };
    use std::io::Read;
    use yserver_protocol::x11::CreateWindowRequest;

    const MARCO: u32 = 14;
    const CHILD_XID: u32 = 0x0010_0101;
    const DAMAGE_XID: u32 = 0x0010_0102;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, MARCO);

    state
        .composite_redirects
        .redirect_subwindows(
            ROOT_WINDOW,
            state.resources.children(ROOT_WINDOW),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(MARCO),
            },
        )
        .unwrap();

    state.resources.create_window(
        ClientId(MARCO),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_XID),
            parent: ROOT_WINDOW,
            x: 100,
            y: 100,
            width: 545,
            height: 204,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .composite_redirects
        .redirect_new_subwindow(ROOT_WINDOW, ResourceId(CHILD_XID));
    let _ = state.resources.map_window(ResourceId(CHILD_XID));

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(MARCO),
            drawable: ResourceId(CHILD_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&CHILD_XID.to_le_bytes());
    body.extend_from_slice(&0x0003u16.to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&250i32.to_le_bytes());
    body.extend_from_slice(&300i32.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(MARCO),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    assert!(
        all.chunks_exact(32)
            .any(|evt| evt[0] == crate::nested::DAMAGE_FIRST_EVENT),
        "move-only ConfigureWindow under inherited RedirectSubwindows \
             must emit DamageNotify even when redirected_backing is not \
             directly populated on the child window",
    );
}

#[test]
fn configure_window_move_on_unmapped_redirected_window_emits_no_damage() {
    // i3 floating-drag smear repro: i3 creates a "floatingcon"
    // frame as a root child, sizes/moves it during the drag, but
    // NEVER maps it. Root is composite-redirected (fastcompmgr),
    // so the child inherits the redirect. A move/resize
    // ConfigureWindow on this UNMAPPED window must NOT emit
    // DamageNotify — Xorg damages only realized/viewable windows on
    // configure. Without the map-state gate, fastcompmgr receives
    // damage for the unmapped floatingcon, NameWindowPixmaps it, and
    // composites its stale backing at the drag position → the trail.
    use crate::{
        resources::ROOT_WINDOW,
        server::{CompositeRedirectMode, DamageObject, RedirectRecord},
    };
    use std::io::Read;
    use yserver_protocol::x11::CreateWindowRequest;

    const FASTCOMPMGR: u32 = 14;
    const FLOATINGCON_XID: u32 = 0x0010_0101;
    const DAMAGE_XID: u32 = 0x0010_0102;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, FASTCOMPMGR);

    state
        .composite_redirects
        .redirect_subwindows(
            ROOT_WINDOW,
            state.resources.children(ROOT_WINDOW),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(FASTCOMPMGR),
            },
        )
        .unwrap();

    state.resources.create_window(
        ClientId(FASTCOMPMGR),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(FLOATINGCON_XID),
            parent: ROOT_WINDOW,
            x: 100,
            y: 100,
            width: 545,
            height: 204,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    // NOTE: deliberately NOT calling map_window — the floatingcon
    // stays Unmapped for its whole life.

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(FASTCOMPMGR),
            drawable: ResourceId(FLOATINGCON_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    // ConfigureWindow move: value-mask x|y (0x0003), new (250, 300).
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&FLOATINGCON_XID.to_le_bytes());
    body.extend_from_slice(&0x0003u16.to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&250i32.to_le_bytes());
    body.extend_from_slice(&300i32.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(FASTCOMPMGR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    assert!(
        !all.chunks_exact(32)
            .any(|evt| evt[0] == crate::nested::DAMAGE_FIRST_EVENT),
        "move ConfigureWindow on an UNMAPPED redirected window must \
             NOT emit DamageNotify (i3 floatingcon smear); Xorg damages \
             only realized windows on configure",
    );
}

#[test]
fn configure_window_stack_only_on_redirected_window_does_not_emit_damage() {
    use crate::{
        resources::{ROOT_WINDOW, RedirectedBacking},
        server::{CompositeRedirectMode, DamageObject, RedirectRecord},
    };
    use std::io::Read;
    use yserver_protocol::x11::CreateWindowRequest;

    const MARCO: u32 = 14;
    const FRAME_XID: u32 = 0x0010_0201;
    const SIBLING_XID: u32 = 0x0010_0202;
    const DAMAGE_XID: u32 = 0x0010_0203;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, MARCO);

    for xid in [FRAME_XID, SIBLING_XID] {
        state.resources.create_window(
            ClientId(MARCO),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(xid),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 5120,
                height: 1440,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(xid));
        state
            .composite_redirects
            .redirect_window(
                ResourceId(xid),
                RedirectRecord {
                    mode: CompositeRedirectMode::Manual,
                    owner: ClientId(MARCO),
                },
            )
            .unwrap();
        if let Some(w) = state.resources.window_mut(ResourceId(xid)) {
            w.host_xid = crate::backend::WindowHandle::from_raw(0xD000_0000 | xid);
            w.redirected_backing = Some(RedirectedBacking {
                host_pixmap: crate::backend::PixmapHandle::from_raw(0xC000_0000 | xid).unwrap(),
                width: 5120,
                height: 1440,
                depth: 24,
            });
        }
    }

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(MARCO),
            drawable: ResourceId(FRAME_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    // value_mask = 0x60 = CWSibling | CWStackMode, no geometry bits.
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&FRAME_XID.to_le_bytes());
    body.extend_from_slice(&0x0060u16.to_le_bytes());
    body.extend_from_slice(&[0u8; 2]);
    body.extend_from_slice(&SIBLING_XID.to_le_bytes());
    body.push(1); // Below
    body.extend_from_slice(&[0u8; 3]);
    process_request(
        &mut state,
        &mut backend,
        ClientId(MARCO),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    peer.set_nonblocking(true).unwrap();
    let mut all = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match peer.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    assert!(
        !all.chunks_exact(32)
            .any(|evt| evt[0] == crate::nested::DAMAGE_FIRST_EVENT),
        "stack-only ConfigureWindow on a redirected window must not emit synthetic \
             DamageNotify; that turns restacks into bogus full-window damage",
    );
}
