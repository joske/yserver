use super::*;

/// GetImage of a window partly off the screen: BadMatch unless the
/// window is redirected, when its bound is its backing (`DoGetImage`,
/// `dix/dispatch.c:2176-2210`; measured by
/// tools/vng-scenarios/draw-clip-probe.c, `F at x=-50`).
#[test]
fn get_image_of_a_redirected_window_off_the_screen_reads_its_backing() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = ResourceId(0x1100);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window,
            parent: ROOT_WINDOW,
            x: -50,
            y: 0,
            width: 200,
            height: 150,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.window_mut(window).unwrap().map_state = MapState::Viewable;
    let mut body = window.0.to_le_bytes().to_vec();
    for v in [0i16, 0] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    for v in [200u16, 150] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut get_image = |state: &mut ServerState| {
        process_request(
            state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: 73,
                data: 2,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            &body,
            None,
        )
        .expect("process_request");
        read_all_available(&mut peer)
    };
    let unredirected = get_image(&mut state);
    assert_eq!(
        (unredirected[0], unredirected[1]),
        (0, x11::error::BAD_MATCH)
    );
    state
        .composite_redirects
        .redirect_window(
            window,
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        )
        .unwrap();
    let redirected = get_image(&mut state);
    assert_eq!(redirected.first(), Some(&1), "a reply, not an error");
}

// ---- L2 plan B.1: COMPOSITE redirect dispatch policy --------

fn composite_redirect_request_body(window: u32, mode: u8) -> Vec<u8> {
    let mut body = vec![0u8; 8];
    body[0..4].copy_from_slice(&window.to_le_bytes());
    body[4] = mode;
    body
}

fn dispatch_composite_redirect(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    client_id: ClientId,
    window: u32,
    mode: u8,
) -> RequestOutcome {
    dispatch_composite_window_update(state, backend, client_id, 1, window, mode)
}

/// Any of Redirect/UnredirectWindow/Subwindows (minor 1-4).
fn dispatch_composite_window_update(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    client_id: ClientId,
    minor: u8,
    window: u32,
    mode: u8,
) -> RequestOutcome {
    let body = composite_redirect_request_body(window, mode);
    process_request(
        state,
        backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 144,
            data: minor,
            length_units: 3,
        },
        &body,
        None,
    )
    .unwrap()
}

/// The error code a client got, or `None` when nothing was written.
fn read_error_code(peer: &mut std::os::unix::net::UnixStream) -> Option<u8> {
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    match peer.read(&mut buf) {
        Ok(32) if buf[0] == 0 => Some(buf[1]),
        Ok(0) | Err(_) => None,
        Ok(n) => panic!("unexpected {n} bytes: {buf:02x?}"),
    }
}

#[test]
fn second_manual_redirect_from_another_client_is_bad_access() {
    let mut state = ServerState::new();
    let mut peer_a = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    create_root_child(&mut state, 0x0010_0001);
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(1), 0x0010_0001, 1);
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(2), 0x0010_0001, 1);
    assert_eq!(read_error_code(&mut peer_a), None);
    assert_eq!(read_error_code(&mut peer_b), Some(x11::error::BAD_ACCESS));
}

#[test]
fn automatic_redirects_from_several_clients_coexist() {
    // Xorg compRedirectWindow refuses only a second Manual redirect.
    let mut state = ServerState::new();
    let mut peer_a = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    create_root_child(&mut state, 0x0010_0001);
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(1), 0x0010_0001, 0);
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(2), 0x0010_0001, 1);
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(2), 0x0010_0001, 0);
    assert_eq!(read_error_code(&mut peer_a), None);
    assert_eq!(read_error_code(&mut peer_b), None);
    assert_eq!(
        state
            .composite_redirects
            .window_records(ResourceId(0x0010_0001))
            .len(),
        3
    );
}

#[test]
fn redirect_of_an_unknown_window_is_bad_window() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(1), 0xDEAD, 1);
    assert_eq!(read_error_code(&mut peer), Some(x11::error::BAD_WINDOW));
    assert!(state.composite_redirects.is_empty());
}

#[test]
fn redirect_window_of_the_root_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(1), ROOT_WINDOW.0, 1);
    assert_eq!(read_error_code(&mut peer), Some(x11::error::BAD_MATCH));
}

#[test]
fn fullscreen_unredirect_and_re_redirect_under_root_subwindows_redirect() {
    // Measured on Xvfb 21.1 (tools/vng-scenarios/composite-reredirect).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let w = 0x0010_0001;
    dispatch_composite_window_update(&mut state, &mut backend, ClientId(1), 2, ROOT_WINDOW.0, 1);
    create_root_child(&mut state, w);
    state
        .composite_redirects
        .redirect_new_subwindow(ROOT_WINDOW, ResourceId(w));
    let codes = [
        (3, None),                         // UnredirectWindow(W)
        (3, Some(x11::error::BAD_VALUE)),  // again
        (1, None),                         // RedirectWindow(W, Manual)
        (1, Some(x11::error::BAD_ACCESS)), // again
        (4, None),                         // UnredirectSubwindows(root)
        (3, Some(x11::error::BAD_VALUE)),  // its free took W's record too
    ];
    for (minor, want) in codes {
        let target = if minor == 4 { ROOT_WINDOW.0 } else { w };
        dispatch_composite_window_update(&mut state, &mut backend, ClientId(1), minor, target, 1);
        assert_eq!(read_error_code(&mut peer), want, "minor {minor}");
    }
    assert!(state.composite_redirects.is_empty());
}

#[test]
fn redirect_window_manual_conflicts_with_inherited_manual() {
    use crate::server::{CompositeRedirectMode, RedirectRecord};
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = ResourceId(0x0100_0042);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window,
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .composite_redirects
        .redirect_subwindows(
            ROOT_WINDOW,
            state.resources.children(ROOT_WINDOW),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        )
        .unwrap();

    // Muffin issues this redundant request for a root child that
    // already inherited root RedirectSubwindows(Manual). Xorg's
    // compRedirectWindow rejects it even for the same client.
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(1), window.0, 1);

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("BadAccess delivered");
    assert_eq!(buf[0], 0);
    assert_eq!(buf[1], x11::error::BAD_ACCESS);
    assert_eq!(
        state.composite_redirects.window_records(window).len(),
        1,
        "rejected probe must not install a second redirect",
    );
}

#[test]
fn redirect_window_cow_is_success_noop() {
    // RedirectWindow(COW) must succeed silently (no X error on the wire)
    // and must NOT install a redirect — matching Xorg compalloc.c:145-147.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state
        .resources
        .materialize_cow_resource(crate::backend::WindowHandle::from_raw_for_test(0xC0C0));

    dispatch_composite_redirect(
        &mut state,
        &mut backend,
        ClientId(1),
        crate::resources::COMPOSITE_OVERLAY_WINDOW.0,
        1, // Manual
    );

    // No X11 error should have been queued to the client.
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    let n = peer.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "no error bytes expected, got {n} bytes: {buf:02x?}");

    // The COW must not be marked redirected: no redirect record exists.
    assert!(
        state.composite_redirects.is_empty(),
        "RedirectWindow(COW) is a Success no-op, COW stays unredirected"
    );
}

// ---------------- end extract_shm_zpixmap_region ----------------

#[test]
fn redirect_invalid_mode_returns_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Anything outside {0, 1} is a spec violation → BadValue.
    create_root_child(&mut state, 0x0010_0001);
    dispatch_composite_redirect(&mut state, &mut backend, ClientId(1), 0x0010_0001, 2);
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error delivered");
    assert_eq!(buf[0], 0);
    // BAD_VALUE = 2
    assert_eq!(buf[1], 2, "expected BadValue (2), got {}", buf[1]);
    assert!(state.composite_redirects.is_empty());
}

#[test]
fn name_window_pixmap_uses_redirected_backing_geometry() {
    use crate::{
        backend::{PixmapHandle, WindowHandle},
        resources::RedirectedBacking,
        server::{CompositeRedirectMode, RedirectRecord},
    };
    use yserver_protocol::x11::CreateWindowRequest;

    const WINDOW: ResourceId = ResourceId(0x10004a);
    const PIXMAP: ResourceId = ResourceId(0x300225);

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new().with_composite_support();
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 32,
            window: WINDOW,
            parent: ROOT_WINDOW,
            width: 646,
            height: 501,
            border_width: 2,
            class: 1,
            visual: crate::resources::ARGB_VISUAL,
            ..Default::default()
        },
    );
    assert!(state.resources.map_window(WINDOW).mapping_changed);
    let window = state
        .resources
        .window_mut(WINDOW)
        .expect("window installed");
    window.host_xid = Some(WindowHandle::from_raw_for_test(0x40008a));
    window.redirected_backing = Some(RedirectedBacking {
        host_pixmap: PixmapHandle::from_raw_for_test(0x4000d),
        width: 650,
        height: 505,
        depth: 32,
    });
    state
        .composite_redirects
        .redirect_window(
            WINDOW,
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        )
        .unwrap();

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&WINDOW.0.to_le_bytes());
    body.extend_from_slice(&PIXMAP.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::NAME_WINDOW_PIXMAP,
        &body,
    );

    let pixmap = state.resources.pixmap(PIXMAP).expect("named pixmap exists");
    assert_eq!((pixmap.width, pixmap.height), (650, 505));
    let aliases = &state
        .resources
        .window(WINDOW)
        .expect("window remains installed")
        .composite_named_pixmaps;
    assert_eq!(aliases.len(), 1);
    assert_eq!((aliases[0].width, aliases[0].height), (650, 505));
}

#[test]
fn get_overlay_window_wires_cow_host_xid() {
    // Pre-condition: COW resource record does NOT exist (post-Task-2.1
    // the pre-seed is gone; COW materialises only on GetOverlayWindow).
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .is_none(),
        "COW must NOT be pre-seeded; the materialise-on-GET model \
             owns the entire COW lifecycle (Task 2.1 + Task 2.4)",
    );

    // GetOverlayWindow body: window xid (we use root, marco ignores
    // the reply's input and just trusts whatever XID we hand back).
    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );

    // Load-bearing assertion: after GET, host_drawable_target must
    // resolve. This is what makes PresentPixmap → COW actually paint.
    let target = state
        .resources
        .host_drawable_target(crate::resources::COMPOSITE_OVERLAY_WINDOW);
    match target {
        Some(crate::resources::HostDrawableTarget::Window {
            nested,
            host_xid,
            depth,
        }) => {
            assert_eq!(nested, crate::resources::COMPOSITE_OVERLAY_WINDOW);
            assert_eq!(
                host_xid.as_raw(),
                crate::resources::COMPOSITE_OVERLAY_WINDOW.0,
                "COW host_xid must equal its protocol xid (0x103)",
            );
            assert_eq!(depth, 24, "COW is depth-24");
        }
        other => panic!(
            "host_drawable_target(COW) must resolve to Window after \
                 GetOverlayWindow — got {other:?}. Pre-fix shape returned \
                 None (host_xid still unset on the resource record), which \
                 dropped every PresentPixmap → COW.",
        ),
    }

    // Dimensions: COW should track the root extent (the backend's
    // COW storage is screen-extent per the Stage 4d plan).
    let cow = state
        .resources
        .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
        .expect("COW record");
    let root = state
        .resources
        .window(crate::resources::ROOT_WINDOW)
        .expect("ROOT record");
    assert_eq!(cow.width, root.width, "COW must match root extent");
    assert_eq!(cow.height, root.height, "COW must match root extent");
}

#[test]
fn repeated_get_reuses_materialized_host_overlay() {
    // A compositor may emit GetOverlayWindow multiple times (e.g.
    // re-registration on a window-manager hand-off). What is stable
    // across those calls is the identity of the already-materialized
    // host overlay: the `host_xid.is_none()` guard prevents repeated
    // allocation and host_xid stays wired.
    //
    // The *request* is deliberately NOT idempotent — each call
    // records another claim in `state.cow_claims` (Xorg's N-record
    // model) and each needs its own ReleaseOverlayWindow.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );

    let cow = state
        .resources
        .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
        .expect("COW record");
    assert_eq!(
        cow.host_xid.map(crate::backend::WindowHandle::as_raw),
        Some(crate::resources::COMPOSITE_OVERLAY_WINDOW.0),
        "host_xid stays wired across repeated GETs",
    );
    assert_eq!(
        state.cow_claims,
        vec![ClientId(1), ClientId(1)],
        "each GET records its own claim — what moved to the core claim \
             list is exactly what the backend refcount used to count",
    );
}

/// Xorg `compCreateOverlayWindow` (`composite/compoverlay.c:149`) gives the
/// overlay no input shape, so it takes the pointer over the whole screen
/// until the compositor empties its input region, and again after a reset
/// to None (measured on Xvfb 21.1, tools/vng-scenarios/cow-input-shape).
#[test]
fn overlay_window_takes_the_pointer_until_its_input_region_is_emptied() {
    use yserver_protocol::x11::{shape as x11shape, xfixes as x11xfixes};

    const APP: ResourceId = ResourceId(0x0020_0001);
    const REGION: u32 = 0x0010_0042;
    let cow = crate::resources::COMPOSITE_OVERLAY_WINDOW;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: APP,
            parent: ROOT_WINDOW,
            x: 100,
            y: 100,
            width: 200,
            height: 200,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(APP);
    let hit = |state: &ServerState| state.root_pointer_target_at(150, 150).map(|h| h.0);
    assert_eq!(hit(&state), Some(APP));

    let mut body = ROOT_WINDOW.0.to_le_bytes().to_vec();
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert_eq!(hit(&state), Some(cow), "an unshaped COW takes the pointer");

    let mut xfixes = |state: &mut ServerState, minor: u8, body: Vec<u8>| {
        let header = yserver_protocol::x11::RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        };
        handle_xfixes_request(
            state,
            &mut backend,
            None,
            ClientId(1),
            SequenceNumber(2),
            header,
            &body,
        )
        .expect("XFIXES request");
    };
    let input_region = |region: u32| {
        let mut body = cow.0.to_le_bytes().to_vec();
        body.extend_from_slice(&[x11shape::KIND_INPUT, 0, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&region.to_le_bytes());
        body
    };
    xfixes(
        &mut state,
        x11xfixes::CREATE_REGION,
        REGION.to_le_bytes().to_vec(),
    );
    xfixes(
        &mut state,
        x11xfixes::SET_WINDOW_SHAPE_REGION,
        input_region(REGION),
    );
    assert_eq!(
        hit(&state),
        Some(APP),
        "an empty input region passes through"
    );
    xfixes(
        &mut state,
        x11xfixes::SET_WINDOW_SHAPE_REGION,
        input_region(0),
    );
    assert_eq!(hit(&state), Some(cow), "None restores the default region");

    body[0..4].copy_from_slice(&cow.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        3,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &body,
    );
    assert_eq!(hit(&state), Some(APP));
}

#[test]
fn overlay_window_create_and_release_notify_root_substructure_listeners() {
    // Xorg compCreateOverlayWindow / DeleteWindow, measured on Xvfb 21.1
    // (tools/vng-scenarios/composite-reredirect): CreateNotify, MapNotify;
    // then UnmapNotify, DestroyNotify after the last release.
    const SUBSTRUCTURE_NOTIFY: u32 = 0x0008_0000;
    const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
    let mut state = ServerState::new();
    let _compositor = install_client(&mut state, 1);
    let mut listener = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    state
        .clients
        .get_mut(&2)
        .expect("listener")
        .event_masks
        .insert(ROOT_WINDOW, SUBSTRUCTURE_NOTIFY);
    let cow = crate::resources::COMPOSITE_OVERLAY_WINDOW;
    let events = |bytes: &[u8]| -> Vec<(u8, u32, u32)> {
        let word =
            |e: &[u8], at: usize| u32::from_le_bytes([e[at], e[at + 1], e[at + 2], e[at + 3]]);
        bytes
            .chunks_exact(32)
            .map(|e| (e[0], word(e, 4), word(e, 8)))
            .collect()
    };
    let root_body = ROOT_WINDOW.0.to_le_bytes();
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &root_body,
    );
    assert_eq!(
        events(&read_all_available(&mut listener)),
        vec![(16, ROOT_WINDOW.0, cow.0), (19, ROOT_WINDOW.0, cow.0)],
        "CreateNotify(parent=root, window=COW), MapNotify(event=root)"
    );
    state
        .clients
        .get_mut(&2)
        .expect("listener")
        .event_masks
        .insert(cow, STRUCTURE_NOTIFY);
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &root_body,
    );
    assert_eq!(
        events(&read_all_available(&mut listener)),
        vec![
            (18, cow.0, cow.0),
            (18, ROOT_WINDOW.0, cow.0),
            (17, cow.0, cow.0),
            (17, ROOT_WINDOW.0, cow.0),
        ],
    );
}

#[test]
fn release_overlay_window_destroys_cow_resource_on_final_release() {
    // Final release (backend returns Ok(true)) must DESTROY the COW
    // resource record entirely (record removed from `windows`, edge
    // removed from `root.children`) so a subsequent `GetOverlayWindow`
    // re-materialises fresh storage via the 0→1 backend path. The
    // pre-Task-2.5 shape merely cleared `host_xid` on the still-existing
    // record; that diverged from Xorg (no record between
    // release-and-next-claim) and silently retained per-COW state
    // (event masks, properties) across the release boundary.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Wire COW via GET first.
    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .and_then(|w| w.host_xid)
            .is_some(),
    );
    let old_generation =
        state.present_window_generation(crate::resources::COMPOSITE_OVERLAY_WINDOW.0);
    state.present_window_msc.insert(
        crate::resources::COMPOSITE_OVERLAY_WINDOW.0,
        crate::server::PresentWindowMsc {
            last_crtc: 2,
            last_crtc_epoch: 1,
            msc_offset: 10,
            last_raw_msc: 20,
        },
    );
    state.present_event_selections.insert(
        0x0001_c0e1,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: crate::resources::COMPOSITE_OVERLAY_WINDOW,
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    state
        .present_pending_msc
        .push(crate::server::PendingNotifyMsc {
            owner: ClientId(1),
            window: crate::resources::COMPOSITE_OVERLAY_WINDOW.0,
            crtc_id: 2,
            crtc_epoch: 1,
            msc_offset: 10,
            serial: 1,
            target_msc: 30,
            divisor: 0,
            remainder: 0,
            byte_order: ClientByteOrder::LittleEndian,
        });
    let stale_event = crate::backend::CompletedPresentEvent {
        client_id: ClientId(1),
        serial: 1,
        host_xid: 0x0001_c002,
        dst_host_xid: crate::resources::COMPOSITE_OVERLAY_WINDOW.0,
        options: 0,
        present_id: 0x_c0,
        window_generation: old_generation,
        crtc_id: 2,
        crtc_epoch: 1,
        msc_offset: 10,
        completion_clock: None,
        wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };

    // Final release — client 1 holds the only claim.
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );
    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .is_none(),
        "final release (backend.Ok(true)) must destroy the COW resource \
             record entirely so the next GetOverlayWindow re-materialises \
             fresh storage",
    );
    assert!(
        !state
            .resources
            .window(crate::resources::ROOT_WINDOW)
            .unwrap()
            .children
            .contains(&crate::resources::COMPOSITE_OVERLAY_WINDOW),
        "final release must remove COW from root.children too",
    );
    assert!(
        !state
            .present_window_generations
            .contains_key(&crate::resources::COMPOSITE_OVERLAY_WINDOW.0)
    );
    assert!(
        !state
            .present_window_msc
            .contains_key(&crate::resources::COMPOSITE_OVERLAY_WINDOW.0)
    );
    assert!(state.present_pending_msc.is_empty());
    assert!(state.present_event_selections.is_empty());

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        3,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    let new_generation =
        state.present_window_generation(crate::resources::COMPOSITE_OVERLAY_WINDOW.0);
    assert_ne!(new_generation, old_generation);
    state.present_event_selections.insert(
        0x0001_c0e2,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: crate::resources::COMPOSITE_OVERLAY_WINDOW,
            event_mask: yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);
    backend.completed_present_events_to_drain.push(stale_event);
    crate::core_loop::run::run_iteration_tail(&mut state, &mut backend);
    assert_eq!(backend.signalled_present_wakes, vec![0x_c0]);
    assert!(read_all_available(&mut peer).is_empty());
}

#[test]
fn release_overlay_window_keeps_host_xid_on_non_final_release() {
    // Non-final release (a claim remains): storage is still live on
    // the backend, host_xid must stay wired so any remaining
    // compositor's PresentPixmap → COW keeps landing.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let _peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    // A second claimant, so client 2's release below is not the final
    // one. Which release is final is core's decision, taken from the
    // claim list — the backend has no say and no counter.
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(2),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(2),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );
    let cow = state
        .resources
        .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
        .expect("COW record");
    assert_eq!(
        cow.host_xid.map(crate::backend::WindowHandle::as_raw),
        Some(crate::resources::COMPOSITE_OVERLAY_WINDOW.0),
        "non-final release must keep host_xid wired — \
             storage is still alive on the backend",
    );
}

/// Get by A, Release by B ⇒ `BadMatch`, and A's claim survives.
/// Xorg's `ProcCompositeReleaseOverlayWindow` looks the caller up
/// with `compFindOverlayClient` and refuses when it holds no record.
#[test]
fn release_overlay_window_by_non_claimant_is_badmatch() {
    let mut state = ServerState::new();
    let _peer_a = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    let _ = read_all_available(&mut peer_b);

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(2),
        1,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );

    assert_error_code(
        &read_all_available(&mut peer_b),
        yserver_protocol::x11::error::BAD_MATCH,
        "ReleaseOverlayWindow from a client holding no claim",
    );
    assert!(
        backend.cow_materialized,
        "B's refused release must not tear the overlay out from under A",
    );
    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .is_some(),
        "A's claim must survive B's refused release",
    );
}

/// `ReleaseOverlayWindow` from a client that never claimed anything,
/// with no claim outstanding anywhere, is `BadMatch` and changes
/// nothing. Xorg answers the same via `compFindOverlayClient`.
#[test]
fn release_overlay_window_with_no_claim_anywhere_is_badmatch() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );

    assert_error_code(
        &read_all_available(&mut peer),
        yserver_protocol::x11::error::BAD_MATCH,
        "unpaired ReleaseOverlayWindow",
    );
    assert!(state.cow_claims.is_empty());
    assert!(
        !backend.cow_materialized,
        "a refused release must not touch the backend at all",
    );
}

/// N Gets need N Releases: Get and Release pair 1:1, as Xorg's
/// per-Get `CompOverlayClientRec` records do. The overlay survives
/// every release but the last.
#[test]
fn repeated_gets_need_matching_releases() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    for seq in 1..=3 {
        dispatch_composite_minor(
            &mut state,
            &mut backend,
            ClientId(1),
            seq,
            yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
            &body,
        );
    }
    assert_eq!(state.cow_claims.len(), 3);

    for seq in 4..=5 {
        dispatch_composite_minor(
            &mut state,
            &mut backend,
            ClientId(1),
            seq,
            yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
            &[],
        );
    }
    assert_eq!(state.cow_claims.len(), 1);
    assert!(
        backend.cow_materialized,
        "two of three claims released — the overlay must still be up",
    );
    let _ = read_all_available(&mut peer);

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        6,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );
    assert!(state.cow_claims.is_empty());
    assert!(
        !backend.cow_materialized,
        "the third release is the final one"
    );
    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .is_none(),
    );

    // A fourth release has nothing left to pair with.
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        7,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );
    assert_error_code(
        &read_all_available(&mut peer),
        yserver_protocol::x11::error::BAD_MATCH,
        "one release too many",
    );
}

/// Two claimants: the first Release is not the final one, so the
/// backend is not called and the overlay stays up for the other
/// claimant.
#[test]
fn two_claimants_first_release_is_not_final() {
    let mut state = ServerState::new();
    let _peer_a = install_client(&mut state, 1);
    let _peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    for client in [1u32, 2] {
        dispatch_composite_minor(
            &mut state,
            &mut backend,
            ClientId(client),
            1,
            yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
            &body,
        );
    }

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );

    assert_eq!(
        state.cow_claims,
        vec![ClientId(2)],
        "A released exactly its own claim; B's is untouched",
    );
    assert!(backend.cow_materialized);

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(2),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );
    assert!(state.cow_claims.is_empty());
    assert!(!backend.cow_materialized);
}

/// Transactional first Get: if materialization fails, the claim is
/// rolled back and the caller gets `BadAlloc`. A claim recorded
/// against an overlay that does not exist is the desynchronisation
/// this model removes.
#[test]
fn first_get_materialization_failure_records_no_claim() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    backend.cow_materialize_fails = true;

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );

    assert_error_code(
        &read_all_available(&mut peer),
        yserver_protocol::x11::error::BAD_ALLOC,
        "GetOverlayWindow whose materialization failed",
    );
    assert!(
        state.cow_claims.is_empty(),
        "the claim must be rolled back — never leave one recorded \
             against an overlay that does not exist",
    );
    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .is_none(),
    );

    // And the server is not poisoned: a retry that succeeds works.
    backend.cow_materialize_fails = false;
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert_eq!(state.cow_claims, vec![ClientId(1)]);
    assert!(backend.cow_materialized);
}

/// The headline case, and the bug at its own level: a compositor
/// takes the overlay and **disconnects without releasing**. Its claim
/// must be gone and the overlay torn down. No reset anywhere in this
/// test.
///
/// Not to be confused with `scene.root_overlay` /
/// `root_overlay_on_disconnect` (`kms/render/backend.rs`), a
/// different concept with a confusingly similar name that
/// `Backend::client_disconnected` already handles correctly —
/// finding that call is what makes this leak look handled.
#[test]
fn disconnect_releases_overlay_claim_and_tears_down_cow() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert_eq!(state.cow_claims, vec![ClientId(1)]);

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));

    assert!(
        state.cow_claims.is_empty(),
        "no claim may outlive its owner",
    );
    assert!(
        !backend.cow_materialized,
        "the departing claimant held the last claim — the backend COW \
             must be torn down",
    );
    assert!(
        state
            .resources
            .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
            .is_none(),
        "the resources-side COW record must come down with it",
    );
}

/// Repeated Gets from one client, then it disconnects: all of its
/// claims go, matching Xorg's N-records model where the resource
/// system frees every `CompOverlayClientRec` the client owns.
#[test]
fn disconnect_releases_all_of_a_clients_repeated_overlay_claims() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    for seq in 1..=3 {
        dispatch_composite_minor(
            &mut state,
            &mut backend,
            ClientId(1),
            seq,
            yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
            &body,
        );
    }
    assert_eq!(state.cow_claims.len(), 3);

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));

    assert!(state.cow_claims.is_empty(), "all three claims must go");
    assert!(!backend.cow_materialized);
}

/// Two claimants: the overlay survives the first departure and comes
/// down on the second.
#[test]
fn overlay_survives_first_claimants_disconnect_and_dies_with_the_second() {
    let mut state = ServerState::new();
    let _peer_a = install_client(&mut state, 1);
    let _peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    for client in [1u32, 2] {
        dispatch_composite_minor(
            &mut state,
            &mut backend,
            ClientId(client),
            1,
            yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
            &body,
        );
    }

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));
    assert_eq!(state.cow_claims, vec![ClientId(2)]);
    assert!(
        backend.cow_materialized,
        "B still holds a claim — the overlay must survive A's departure",
    );

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(2));
    assert!(state.cow_claims.is_empty());
    assert!(!backend.cow_materialized);
}

/// `KillClient` on another client's resource calls
/// `process_disconnect` inline, bypassing
/// `disconnect_with_pending_cleanup`. That is exactly why the claim
/// release lives in `process_disconnect` and not in the funnel.
#[test]
fn kill_client_releases_the_victims_overlay_claim() {
    let mut state = ServerState::new();
    let _peer_a = install_client(&mut state, 1);
    let _peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    // Client 2 owns a window, so client 1 can name it in KillClient.
    let victim_window = ResourceId(0x2000_0001);
    state.resources.create_window(
        ClientId(2),
        CreateWindowRequest {
            depth: 24,
            window: victim_window,
            parent: ROOT_WINDOW,
            width: 64,
            height: 64,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(2),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert_eq!(state.cow_claims, vec![ClientId(2)]);

    handle_kill_client(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        &victim_window.0.to_le_bytes(),
    )
    .expect("KillClient");

    assert!(
        !state.clients.contains_key(&2),
        "precondition: the victim was force-disconnected",
    );
    assert!(
        state.cow_claims.is_empty(),
        "the killed compositor's claim must go with it — KillClient \
             bypasses disconnect_with_pending_cleanup, so the release has \
             to live in process_disconnect",
    );
    assert!(!backend.cow_materialized);
}

/// A `RetainPermanent` claimant's overlay claims are released anyway
/// when its connection goes. The overlay is a screen-wide singleton;
/// a zombie holding it would block every future compositor.
#[test]
fn retained_clients_overlay_claims_are_released_anyway() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // RetainPermanent.
    state.close_down_modes.insert(1, 1);

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));

    assert!(
        state.zombie_clients.contains_key(&1),
        "precondition: the client was retained, not destroyed",
    );
    assert!(
        state.cow_claims.is_empty(),
        "a claim is not a retainable resource",
    );
    assert!(!backend.cow_materialized);
}

/// Protocol path, final release, teardown fails: `BadAlloc`, and the
/// caller **keeps** its claim — the release did not happen, so a
/// compositor can retry. Xorg maps allocation failure in this
/// extension to `BadAlloc` (`composite/compext.c:216,220,263,297`),
/// and a shadow-buffer allocation failing is exactly that.
///
/// The pre-fix handler swallowed the backend `Err` into `false` and
/// reported protocol success for a teardown that did not occur.
#[test]
fn final_release_teardown_failure_is_badalloc_and_keeps_the_claim() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    let _ = read_all_available(&mut peer);
    backend.cow_teardown_fails = true;

    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );

    assert_error_code(
        &read_all_available(&mut peer),
        yserver_protocol::x11::error::BAD_ALLOC,
        "ReleaseOverlayWindow whose teardown failed",
    );
    assert_eq!(
        state.cow_claims,
        vec![ClientId(1)],
        "the caller keeps its claim — the claim is what keeps the \
             overlay alive, so dropping it here is precisely the leak",
    );
    assert!(backend.cow_materialized, "the overlay is still up");
    assert!(
        !state.cow_teardown_failed,
        "the protocol path is retryable: no session-fatal state",
    );

    // And the retry works.
    backend.cow_teardown_fails = false;
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        3,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &[],
    );
    assert!(state.cow_claims.is_empty());
    assert!(!backend.cow_materialized);
}

/// Disconnect path, final release, teardown fails: the claims are
/// released **and** `cow_teardown_failed` is set. The two are not
/// alternatives — a leftover claim would be indistinguishable from a
/// live one and the next compositor would wait on a dead client.
///
/// A later `GetOverlayWindow` from a fresh client is then `BadAlloc`:
/// whatever it would receive is inherited from a session that could
/// not be torn down.
#[test]
fn disconnect_teardown_failure_sets_cow_teardown_failed() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    backend.cow_teardown_fails = true;

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(1));

    assert!(
        state.cow_claims.is_empty(),
        "no claim outlives its owner, teardown failure or not",
    );
    assert!(
        state.cow_teardown_failed,
        "the orphaned overlay needs an owner that is not a claim",
    );
    assert!(
        backend.cow_materialized,
        "the overlay really is still up — that is the whole problem",
    );

    let _ = read_all_available(&mut peer_b);
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(2),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert_error_code(
        &read_all_available(&mut peer_b),
        yserver_protocol::x11::error::BAD_ALLOC,
        "GetOverlayWindow under cow_teardown_failed",
    );
    assert!(
        state.cow_claims.is_empty(),
        "the refused Get records no claim",
    );
}

// ---------------- rotate_redirected_backing_on_resize ----------------
//
// Marco-with-compositing resizes the top mate-panel from 25 → 28 px
// tall after `RedirectSubwindows(root, Manual)`. The subsequent
// `NameWindowPixmap(panel)` must succeed against fresh storage at
// the new size. Pre-fix v2 returned `NotFound` because the rotate
// path allocated-then-released against an idempotent
// `host_window_to_backing[W]` cache and freed the very pixmap it
// had just chosen to keep. This test pins release-then-allocate
// ORDER — flipping it back would regress the hardware smoke bug.
#[test]
fn rotate_redirected_backing_on_resize_releases_old_then_allocates_new() {
    use crate::backend::recording::RecordedCall;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;
    const OLD_BACKING: u32 = 0x0050_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Install a top-level child of root, then prime it with a
    // pretend-existing redirected backing (the state we'd be in
    // after `RedirectSubwindows` + activation but before the
    // resize). 100x50 → resize target 100x75.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 32,
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
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(OLD_BACKING),
            width: 100,
            height: 50,
            depth: 32,
        });
    }

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        100,
        75,
        false,
        0,
    );

    let calls = backend.calls();
    // RetainBackingStorage comes first to keep OLD's storage
    // alive across the release→copy gap (see
    // `rotate_redirected_backing_retains_old_storage_across_release_then_drops_after_copy`).
    // Release must then precede Allocate so the backend's
    // idempotent `host_window_to_backing[W]` lookup misses on
    // the subsequent allocate.
    assert_eq!(
        calls[0],
        RecordedCall::RetainBackingStorage(OLD_BACKING),
        "step 1 must be RetainBackingStorage on OLD — keeps storage \
             alive across the release→copy gap so the no-alias case \
             doesn't drop the rotate copy",
    );
    assert_eq!(
        calls[1],
        RecordedCall::ReleaseRedirectedBacking(OLD_BACKING),
        "step 2 must be ReleaseRedirectedBacking on the OLD backing — \
             release-before-allocate is load-bearing (see fn doc-comment)",
    );
    assert_eq!(
        calls[2],
        RecordedCall::AllocateRedirectedBacking {
            host_window: HOST_XID,
            width: 100,
            height: 75,
            depth: 32,
        },
        "step 3 must be AllocateRedirectedBacking at the NEW size",
    );

    // Window resource now points at the freshly-allocated backing
    // (RecordingBackend hands back fresh handles per allocate).
    let backing = state
        .resources
        .window(ResourceId(WINDOW_XID))
        .and_then(|w| w.redirected_backing)
        .expect("redirected_backing repointed after rotate");
    assert_ne!(
        backing.host_pixmap.as_raw(),
        OLD_BACKING,
        "redirected_backing must point at the NEW pixmap, not the old one",
    );
    assert_eq!(backing.width, 100);
    assert_eq!(backing.height, 75);
    assert_eq!(backing.depth, 32);
}

/// How a GLX-TFP compositor lets go of a named, resized window.
#[derive(Clone, Copy, Debug)]
enum GlxTeardown {
    DestroyGlxThenFree,
    FreeThenDestroyGlx,
    DestroyWindowFirst,
    Disconnect,
}

fn drive(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data,
            length_units,
        },
        body,
        None,
    )
    .expect("process_request");
}

/// picom-glx under a WM: NameWindowPixmap + glXCreatePixmap, `resizes` resizes, then teardown.
fn glx_export_ref_after_resizes(resizes: u16, teardown: GlxTeardown) -> RecordingBackend {
    use yserver_protocol::x11::glx as x11glx;
    const WIN: u32 = 0x0077_0001;
    const PIX: u32 = 0x0077_0002;
    const GLXPIX: u32 = 0x0077_0003;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new()
        .with_composite_support()
        .with_redirect_activation();
    let mut cw = Vec::new();
    cw.extend_from_slice(&WIN.to_le_bytes());
    cw.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    cw.extend_from_slice(&[0, 0, 0, 0]); // x, y
    cw.extend_from_slice(&100u16.to_le_bytes());
    cw.extend_from_slice(&50u16.to_le_bytes());
    cw.extend_from_slice(&0u16.to_le_bytes()); // border
    cw.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
    cw.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
    cw.extend_from_slice(&0u32.to_le_bytes()); // no values
    drive(&mut state, &mut backend, 1, 0, &cw);
    let mut redirect = ROOT_WINDOW.0.to_le_bytes().to_vec();
    redirect.extend_from_slice(&[1, 0, 0, 0]); // Manual
    drive(
        &mut state,
        &mut backend,
        144,
        yserver_protocol::x11::composite::REDIRECT_SUBWINDOWS,
        &redirect,
    );
    drive(&mut state, &mut backend, 8, 0, &WIN.to_le_bytes());
    let mut name = WIN.to_le_bytes().to_vec();
    name.extend_from_slice(&PIX.to_le_bytes());
    drive(
        &mut state,
        &mut backend,
        144,
        yserver_protocol::x11::composite::NAME_WINDOW_PIXMAP,
        &name,
    );
    let mut glx = Vec::new();
    glx.extend_from_slice(&0u32.to_le_bytes()); // screen
    glx.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
    glx.extend_from_slice(&PIX.to_le_bytes());
    glx.extend_from_slice(&GLXPIX.to_le_bytes());
    drive(&mut state, &mut backend, 148, x11glx::CREATE_PIXMAP, &glx);
    let first = state
        .resources
        .pixmap(ResourceId(PIX))
        .and_then(|p| p.host_xid)
        .expect("named pixmap")
        .as_raw();
    assert_eq!(
        backend.glx_pixmap_exports.get(&first),
        Some(&1),
        "glXCreatePixmap holds one export ref on the named backing",
    );

    for i in 1..=resizes {
        let mut cfg = WIN.to_le_bytes().to_vec();
        cfg.extend_from_slice(&0x0cu16.to_le_bytes());
        cfg.extend_from_slice(&0u16.to_le_bytes());
        cfg.extend_from_slice(&u32::from(100 + 10 * i).to_le_bytes());
        cfg.extend_from_slice(&u32::from(50 + 10 * i).to_le_bytes());
        drive(&mut state, &mut backend, 12, 0, &cfg);
        let current = state
            .resources
            .window(ResourceId(WIN))
            .and_then(|w| w.redirected_backing)
            .expect("redirected backing")
            .host_pixmap
            .as_raw();
        assert_ne!(current, first, "resize {i} rotated the backing");
        assert_eq!(
            state
                .glx_drawables
                .get(&GLXPIX)
                .and_then(|d| d.glx_export_host_xid),
            Some(current),
            "resize {i}: the GLX pixmap follows the retargeted alias",
        );
        assert_eq!(
            backend
                .glx_pixmap_exports
                .clone()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![(current, 1)],
            "resize {i}: the export ref lives on the backing the GLX pixmap names",
        );
    }

    let destroy_glx = |state: &mut ServerState, backend: &mut RecordingBackend| {
        drive(
            state,
            backend,
            148,
            x11glx::DESTROY_PIXMAP,
            &GLXPIX.to_le_bytes(),
        );
    };
    let free = |state: &mut ServerState, backend: &mut RecordingBackend| {
        drive(state, backend, 54, 0, &PIX.to_le_bytes());
    };
    let destroy_win = |state: &mut ServerState, backend: &mut RecordingBackend| {
        drive(state, backend, 4, 0, &WIN.to_le_bytes());
    };
    match teardown {
        GlxTeardown::DestroyGlxThenFree => {
            destroy_glx(&mut state, &mut backend);
            free(&mut state, &mut backend);
            destroy_win(&mut state, &mut backend);
        }
        GlxTeardown::FreeThenDestroyGlx => {
            free(&mut state, &mut backend);
            destroy_glx(&mut state, &mut backend);
            destroy_win(&mut state, &mut backend);
        }
        GlxTeardown::DestroyWindowFirst => {
            destroy_win(&mut state, &mut backend);
            destroy_glx(&mut state, &mut backend);
            free(&mut state, &mut backend);
        }
        GlxTeardown::Disconnect => {
            crate::core_loop::process_disconnect::process_disconnect(
                &mut state,
                &mut backend,
                ClientId(1),
            );
        }
    }
    backend
}

/// A resize retargets picom's GLX pixmap; the export ref must move with it, or OLD leaks.
#[test]
fn glx_pixmap_export_ref_follows_the_resize_retarget() {
    for resizes in [0, 1, 2] {
        for teardown in [
            GlxTeardown::DestroyGlxThenFree,
            GlxTeardown::FreeThenDestroyGlx,
            GlxTeardown::DestroyWindowFirst,
            GlxTeardown::Disconnect,
        ] {
            let backend = glx_export_ref_after_resizes(resizes, teardown);
            assert!(
                backend.glx_pixmap_exports.is_empty(),
                "resizes={resizes} {teardown:?}: export refs left behind: {:?}",
                backend.glx_pixmap_exports,
            );
        }
    }
}

// compCopyWindow analog: when a redirected window resizes, the
// pre-existing backing's contents must be carried over into the new
// backing for the overlap region — otherwise any compositor that
// re-Names the post-resize backing (marco on mate-panel-top during
// the 25→28-px grow, etc.) samples an empty buffer and the panel
// renders without the icons that were already painted into the old
// backing. Xorg lands this via `compReallocPixmap` (save old
// pixmap) + `compCopyWindow` (copy_area old→new). See
// /home/jos/Projects/xserver/composite/compalloc.c:680-712 and
// compwindow.c:376-388.
#[test]
fn rotate_redirected_backing_on_resize_copies_old_contents_to_new() {
    use crate::backend::recording::RecordedCall;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;
    const OLD_BACKING: u32 = 0x0050_0001;
    const OLD_W: u16 = 100;
    const OLD_H: u16 = 50;
    const NEW_W: u16 = 100;
    const NEW_H: u16 = 75;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 32,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: OLD_W,
            height: OLD_H,
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
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(OLD_BACKING),
            width: OLD_W,
            height: OLD_H,
            depth: 32,
        });
    }

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        NEW_W,
        NEW_H,
        false,
        0,
    );

    // NEW handle is whatever the RecordingBackend's allocate
    // handed back; pull it from the resource state.
    let new_backing = state
        .resources
        .window(ResourceId(WINDOW_XID))
        .and_then(|w| w.redirected_backing)
        .expect("redirected_backing repointed after rotate");
    let new_raw = new_backing.host_pixmap.as_raw();

    let calls = backend.calls();
    // Expected overlap = min(old, new) per dimension.
    let expected_w = OLD_W.min(NEW_W);
    let expected_h = OLD_H.min(NEW_H);
    let copy = calls.iter().find(|c| {
        matches!(
            c,
            RecordedCall::CopyArea {
                src_host_xid,
                dst_host_xid,
                src_x: 0,
                src_y: 0,
                dst_x: 0,
                dst_y: 0,
                width,
                height,
            } if *src_host_xid == OLD_BACKING
                && *dst_host_xid == new_raw
                && *width == expected_w
                && *height == expected_h
        )
    });
    assert!(
        copy.is_some(),
        "expected CopyArea(OLD=0x{OLD_BACKING:x} → NEW=0x{new_raw:x}, \
             src=(0,0), dst=(0,0), {expected_w}x{expected_h}) — \
             missing the compCopyWindow analog that carries pre-resize \
             contents into the freshly-allocated backing. Calls: {calls:?}",
    );
}

// #143 (rendering half) — a SHRINK must rotate exactly like a grow.
//
// Measured under awesome+picom: switching tiling layout shrank a
// mate-terminal frame (`bw = 2`) from 1278x704 to 1276x704 and the
// render module logged NOTHING — `redirected_backing_can_fit` was a
// high-water-mark test, so the 1282x708 backing was kept and only its
// metadata was rewritten. The backing's ring stayed laid out for the
// OLD 1278-px content (2-px ring at columns 1280..1281) and nothing
// re-seeded it, leaving an alpha-0 band where content should be.
//
// Xorg reallocates on inequality in EITHER direction:
// `compReallocPixmap` (../xserver/composite/compalloc.c:698), whose
// replacement is always freshly seeded from the parent
// (`compNewPixmap`, compalloc.c:539-605).
//
// The backend predicate is where the fix lives (its own unit test is
// `redirected_backing_reuse_requires_the_exact_storage_extent`);
// what this test pins is the core half — that the realloc path
// allocates at the NEW bordered extent and carries only the CONTENT
// overlap, leaving the freshly painted ring of NEW alone.
#[test]
fn rotate_redirected_backing_on_shrink_allocates_the_new_bordered_extent() {
    use crate::backend::recording::RecordedCall;

    const BW: u16 = 2;
    // Content 1278x704 → backing 1282x708 (the pre-switch state).
    const OLD_BACKING_W: u16 = 1282;
    const OLD_BACKING_H: u16 = 708;
    // Shrink to content 1276x704 → backing 1280x708.
    const NEW_CONTENT_W: u16 = 1276;
    const NEW_CONTENT_H: u16 = 704;

    let (mut state, mut backend, old_backing) =
        redirected_window_fixture(OLD_BACKING_W, OLD_BACKING_H, BW);

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(ROTATE_WINDOW_XID),
        NEW_CONTENT_W,
        NEW_CONTENT_H,
        false,
        BW,
    );

    let new_backing = state
        .resources
        .window(ResourceId(ROTATE_WINDOW_XID))
        .and_then(|w| w.redirected_backing)
        .expect("redirected_backing repointed after the shrink rotate");
    assert_ne!(
        new_backing.host_pixmap.as_raw(),
        old_backing,
        "a shrink must rotate onto a FRESH backing, not keep the oversized one",
    );
    assert_eq!(
        (new_backing.width, new_backing.height),
        (1280, 708),
        "the new backing is the new BORDERED extent (1276 + 2*2, 704 + 2*2)",
    );

    let calls = backend.calls();
    assert!(
        calls.contains(&RecordedCall::AllocateRedirectedBacking {
            host_window: ROTATE_HOST_XID,
            width: 1280,
            height: 708,
            depth: 32,
        }),
        "the shrink must allocate storage at the new bordered extent \
             1280x708, not keep 1282x708. Calls: {calls:?}",
    );

    // Content-only copy: both backings hold content at `(bw, bw)`,
    // and the overlap is min(1278, 1276) x min(704, 704).
    let new_raw = new_backing.host_pixmap.as_raw();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::CopyArea {
                src_host_xid,
                dst_host_xid,
                src_x: 2,
                src_y: 2,
                dst_x: 2,
                dst_y: 2,
                width: 1276,
                height: 704,
            } if *src_host_xid == old_backing && *dst_host_xid == new_raw
        )),
        "the rotate copy must carry the CONTENT overlap inset by the \
             border width — a full-extent copy would repaint NEW's \
             right/bottom ring columns with OLD's interior pixels. \
             Calls: {calls:?}",
    );
}

// The over-correction guard for the test above: a bordered GROW must
// keep rotating the way it always did, and its copy is inset the same
// way (the ring of NEW is painted by `allocate_redirected_backing`;
// Xorg repaints it too, via the `compRepaintBorder` work proc queued
// from `compSetPixmap`, ../xserver/composite/compwindow.c:137-139).
#[test]
fn rotate_redirected_backing_on_bordered_grow_still_rotates_and_copies_content() {
    use crate::backend::recording::RecordedCall;

    const BW: u16 = 2;
    // Content 1276x704 → backing 1280x708, growing to content
    // 1278x704 → backing 1282x708.
    const OLD_BACKING_W: u16 = 1280;
    const OLD_BACKING_H: u16 = 708;
    const NEW_CONTENT_W: u16 = 1278;
    const NEW_CONTENT_H: u16 = 704;

    let (mut state, mut backend, old_backing) =
        redirected_window_fixture(OLD_BACKING_W, OLD_BACKING_H, BW);

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(ROTATE_WINDOW_XID),
        NEW_CONTENT_W,
        NEW_CONTENT_H,
        false,
        BW,
    );

    let new_backing = state
        .resources
        .window(ResourceId(ROTATE_WINDOW_XID))
        .and_then(|w| w.redirected_backing)
        .expect("redirected_backing repointed after the grow rotate");
    assert_eq!((new_backing.width, new_backing.height), (1282, 708));

    let calls = backend.calls();
    assert!(
        calls.contains(&RecordedCall::AllocateRedirectedBacking {
            host_window: ROTATE_HOST_XID,
            width: 1282,
            height: 708,
            depth: 32,
        }),
        "the grow must allocate at the new bordered extent. Calls: {calls:?}",
    );
    let new_raw = new_backing.host_pixmap.as_raw();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::CopyArea {
                src_host_xid,
                dst_host_xid,
                src_x: 2,
                src_y: 2,
                dst_x: 2,
                dst_y: 2,
                width: 1276,
                height: 704,
            } if *src_host_xid == old_backing && *dst_host_xid == new_raw
        )),
        "the grow copy carries the same content overlap, inset by the \
             border width. Calls: {calls:?}",
    );
}

const ROTATE_WINDOW_XID: u32 = 0x0010_0001;
const ROTATE_HOST_XID: u32 = 0x0040_0001;

/// A root child already redirected, with a backing whose recorded
/// extent is `(backing_w, backing_h)` — i.e. the post-activation
/// state, before the resize under test. Returns the OLD backing's
/// raw xid alongside the fixture.
fn redirected_window_fixture(
    backing_w: u16,
    backing_h: u16,
    border_width: u16,
) -> (ServerState, RecordingBackend, u32) {
    const OLD_BACKING: u32 = 0x0050_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 32,
            window: ResourceId(ROTATE_WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: backing_w.saturating_sub(border_width.saturating_mul(2)),
            height: backing_h.saturating_sub(border_width.saturating_mul(2)),
            border_width,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(ROTATE_WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            ROTATE_HOST_XID,
        ));
        w.border_width = border_width;
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(OLD_BACKING),
            width: backing_w,
            height: backing_h,
            depth: 32,
        });
    }
    (state, backend, OLD_BACKING)
}

// Storage-alive invariant for the rotate copy. Observed in HW
// smoke (yserver-hw-mate.log 17:28:10Z) that the rotate path
// dropped tiny 1×1 CopyAreas with `copy_area dropped — src
// unknown` whenever OLD had no `NameWindowPixmap` aliases: the
// existing `release_redirected_backing` decref'd alias_registry
// to 0 → `free_pixmap` → store entry gone → `store.lookup(OLD)`
// returned None at the copy site → silent drop.
//
// Fix shape: take a rotate-scoped retain on OLD's storage
// BEFORE the release (so release's decref doesn't hit 0), drop
// it AFTER the copy (frees storage only if no other aliases
// hold it). This test pins the call ordering on
// RecordingBackend; the v2-side alias_registry incref/decref
// semantics are exercised by the v2 backend's own tests.
#[test]
fn rotate_redirected_backing_retains_old_storage_across_release_then_drops_after_copy() {
    use crate::backend::recording::RecordedCall;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;
    const OLD_BACKING: u32 = 0x0050_0001;
    const OLD_W: u16 = 1;
    const OLD_H: u16 = 1;
    const NEW_W: u16 = 1;
    const NEW_H: u16 = 1;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 32,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: OLD_W,
            height: OLD_H,
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
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(OLD_BACKING),
            width: OLD_W,
            height: OLD_H,
            depth: 32,
        });
    }

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        NEW_W,
        NEW_H,
        true,
        0,
    );

    let calls = backend.calls();
    let retain_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::RetainBackingStorage(x) if *x == OLD_BACKING));
    let release_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::ReleaseRedirectedBacking(x) if *x == OLD_BACKING));
    let allocate_idx = calls.iter().position(|c| {
        matches!(c, RecordedCall::AllocateRedirectedBacking { host_window, .. } if *host_window == HOST_XID)
    });
    let copy_idx = calls.iter().position(|c| {
        matches!(c, RecordedCall::CopyArea { src_host_xid, .. } if *src_host_xid == OLD_BACKING)
    });
    let drop_idx = calls
        .iter()
        .position(|c| matches!(c, RecordedCall::DropBackingStorage(x) if *x == OLD_BACKING));

    let r = retain_idx.expect(
        "rotate must retain OLD storage before releasing — otherwise the no-alias \
             case frees storage before copy_area can read it (observed as `copy_area dropped \
             — src unknown` in HW smoke)",
    );
    let rel = release_idx.expect("rotate must still release OLD's W→B map slot");
    let a = allocate_idx.expect("rotate must allocate NEW");
    let c = copy_idx.expect("rotate must copy OLD→NEW");
    let d = drop_idx.expect(
        "rotate must drop the retain after copy — leaving storage alive forever \
             would leak the backing across every resize",
    );

    assert!(
        r < rel,
        "Retain(idx={r}) must precede Release(idx={rel}) — release's decref would \
             otherwise hit refcount=0 and free OLD's storage before copy. Calls: {calls:?}",
    );
    assert!(
        rel < a,
        "Release(idx={rel}) must precede Allocate(idx={a}) — host_window_to_backing[W] \
             idempotency would otherwise return OLD instead of allocating fresh. Calls: {calls:?}",
    );
    assert!(
        a < c,
        "Allocate(idx={a}) must precede Copy(idx={c}) — copy needs NEW as destination. \
             Calls: {calls:?}",
    );
    assert!(
        c < d,
        "Copy(idx={c}) must precede Drop(idx={d}) — dropping the retain before the copy \
             defeats the purpose of taking it. Calls: {calls:?}",
    );
}

#[test]
fn rotate_redirected_backing_on_move_forces_reallocate_even_when_size_matches() {
    use crate::backend::recording::RecordedCall;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;
    const OLD_BACKING: u32 = 0x0050_0001;
    const W: u16 = 100;
    const H: u16 = 50;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 32,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: W,
            height: H,
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
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(OLD_BACKING),
            width: W,
            height: H,
            depth: 32,
        });
    }

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        W,
        H,
        true,
        0,
    );

    let calls = backend.calls();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::ReleaseRedirectedBacking(x) if *x == OLD_BACKING
        )),
        "forced move-rotate must release OLD even when size matches; got {calls:?}",
    );
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::AllocateRedirectedBacking {
                host_window,
                width,
                height,
                depth: 32,
            } if *host_window == HOST_XID && *width == W && *height == H
        )),
        "forced move-rotate must allocate a fresh backing at the same size; got {calls:?}",
    );
    let new_backing = state
        .resources
        .window(ResourceId(WINDOW_XID))
        .and_then(|w| w.redirected_backing)
        .expect("redirected_backing repointed after forced rotate");
    assert_ne!(
        new_backing.host_pixmap.as_raw(),
        OLD_BACKING,
        "forced move-rotate must repoint to a fresh backing even at identical size",
    );
}

/// Resize rotates the redirected backing by releasing the old
/// backing and allocating a new one. The fresh allocation must
/// preserve the effective redirect mode's scene-participation
/// flags; otherwise a Manual-redirected frame can become visible
/// through normal scene traversal after a ConfigureWindow resize.
#[test]
fn rotate_redirected_backing_preserves_manual_scene_participation() {
    use crate::backend::recording::RecordedCall;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;
    const OLD_BACKING: u32 = 0x0050_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 32,
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
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(OLD_BACKING),
            width: 100,
            height: 50,
            depth: 32,
        });
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

    rotate_redirected_backing_on_resize(
        &mut state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        100,
        75,
        false,
        0,
    );

    let calls = backend.calls();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetWindowSceneParticipation {
                host_window: HOST_XID,
                participating: false,
            }
        )),
        "rotate must reapply Manual W scene_participating=false; got {calls:?}",
    );
    // The fresh backing must be scene-participating so the
    // scene's `redirected_target` damage-peek sees paints into
    // it post-resize — same invariant as
    // `manual_redirect_marks_backing_scene_participating_so_paints_emit_damage`.
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::SetBackingSceneParticipation {
                participating: true,
                ..
            }
        )),
        "rotate must mark the fresh Manual backing scene-participating; got {calls:?}",
    );
}

// ────────────────────────────────────────────────────────────
// Phase 2 reparent-redirect-reconciliation pins (Task 7).
//
// Mirrors Xorg's `compReparentWindow` (xserver/composite/
// compwindow.c:453-454) — which dispatches to
// `compUnredirectOneSubwindow` (revoke inherited redirect),
// `compRedirectOneSubwindow` (grant inherited redirect), and
// `flip_redirect_target_mode` (mode flip across redirected
// parents). Direct RedirectWindow(W) is per-window so the
// backing is not touched by reparent.
//
// The fifth, user-visible pin lives in
// `crates/yserver/src/kms/render/backend.rs` — it drives the v2
// `resolve_paint_target` ancestor walk through the full
// `process_request` dispatcher.
// ────────────────────────────────────────────────────────────

fn make_test_state() -> ServerState {
    ServerState::new()
}

/// Attach a `RedirectedBacking` to an already-seeded window.
/// The `backend` argument keeps the helper's signature
/// symmetrical with the (eventual) production path which
/// allocates a real backing through the backend; the recording
/// backend doesn't need to be consulted for the test
/// pre-condition, so we just synthesise a PixmapHandle.
fn seed_redirected_window(
    state: &mut ServerState,
    _backend: &mut crate::backend::recording::RecordingBackend,
    xid: ResourceId,
) {
    use crate::resources::RedirectedBacking;
    if let Some(w) = state.resources.window_mut(xid) {
        w.redirected_backing = Some(RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw(0x9000_0000 | xid.0)
                .expect("non-zero PixmapHandle"),
            width: w.width,
            height: w.height,
            depth: w.depth,
        });
    }
}

/// Drive a ReparentWindow through `handle_reparent_window` (the
/// module-private dispatch path) via `process_request`'s public
/// entry. ReparentWindow body = window(4 LE) + parent(4 LE) +
/// x(2 LE i16) + y(2 LE i16) = 12 bytes; length_units = 4
/// (header + body = 16 bytes / 4).
fn dispatch_reparent_window(
    state: &mut ServerState,
    backend: &mut crate::backend::recording::RecordingBackend,
    window: ResourceId,
    parent: ResourceId,
    x: i16,
    y: i16,
) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&parent.0.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    process_request(
        state,
        backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 7, // ReparentWindow
            data: 0,
            length_units: 4,
        },
        &body,
        None,
    )
    .expect("process_request(ReparentWindow) must succeed");
}

#[test]
fn reparent_out_of_redirected_subtree_revokes_inherited_redirect() {
    // Phase 2: mirrors compUnredirectOneSubwindow in
    // /home/jos/Projects/xserver/composite/compwindow.c:453.
    // A window that inherited redirect from its parent's
    // RedirectSubwindows must lose its backing when reparented
    // to a parent without RedirectSubwindows.
    //
    // The actual regression: nm-applet (created as a direct
    // child of root with RedirectSubwindows(root, Manual)
    // active, then reparented into mate-panel's notification
    // socket) keeps a stale backing. Paints land there instead
    // of in mate-panel's pixmap; the compositor reads mate-
    // panel's pixmap and sees an empty tray area.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let socket_xid = ResourceId(0x210_0013);
    let nm_applet_xid = ResourceId(0x180_000b);

    // root has RedirectSubwindows(Manual); socket does not.
    state
        .composite_redirects
        .redirect_subwindows(
            root_xid,
            state.resources.children(root_xid),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: ClientId(14),
            },
        )
        .unwrap();

    seed_window(&mut state, mate_panel_xid, root_xid, 2560, 28);
    seed_redirected_window(&mut state, &mut backend, mate_panel_xid);
    seed_window(&mut state, socket_xid, mate_panel_xid, 26, 27);
    seed_window(&mut state, nm_applet_xid, root_xid, 26, 27);
    seed_redirected_window(&mut state, &mut backend, nm_applet_xid);

    assert!(
        state
            .resources
            .window(nm_applet_xid)
            .unwrap()
            .redirected_backing
            .is_some(),
        "pre-condition: nm-applet has an inherited-redirect backing"
    );

    dispatch_reparent_window(&mut state, &mut backend, nm_applet_xid, socket_xid, 0, 0);

    assert_eq!(
        state.resources.window(nm_applet_xid).unwrap().parent,
        socket_xid,
    );
    assert!(
        state
            .resources
            .window(nm_applet_xid)
            .unwrap()
            .redirected_backing
            .is_none(),
        "post-condition: nm-applet's inherited-redirect backing is freed after \
             reparenting out of the redirected subtree"
    );
}

#[test]
fn muffin_manual_probe_cannot_pin_redirect_across_reparent() {
    // Live Warframe transition: while temporarily a root child, W
    // inherits RedirectSubwindows(root, Manual). Muffin then probes
    // RedirectWindow(W, Manual), which Xorg rejects with BadAccess.
    // If accepted, that direct record makes the subsequent reparent
    // into Muffin's frame skip inherited-redirect teardown and W keeps
    // painting a stale inner backing that Muffin never samples.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();
    let root = crate::resources::ROOT_WINDOW;
    let frame = ResourceId(0x510_0040);
    let window = ResourceId(0x530_0003);

    state
        .composite_redirects
        .redirect_subwindows(
            root,
            state.resources.children(root),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: ClientId(14),
            },
        )
        .unwrap();
    seed_window(&mut state, frame, root, 1024, 768);
    seed_window(&mut state, window, root, 1024, 768);
    seed_redirected_window(&mut state, &mut backend, window);

    dispatch_composite_redirect(&mut state, &mut backend, ClientId(14), window.0, 1);
    assert_eq!(
        state.composite_redirects.window_records(window).len(),
        1,
        "BadAccess probe must not add a redirect",
    );

    dispatch_reparent_window(&mut state, &mut backend, window, frame, 0, 0);
    assert!(
        state
            .resources
            .window(window)
            .unwrap()
            .redirected_backing
            .is_none(),
        "reparent into Muffin frame must revoke W's inherited Manual backing",
    );
}

#[test]
fn reparent_into_redirected_subtree_grants_inherited_redirect() {
    // Phase 2: mirrors compRedirectOneSubwindow at
    // /home/jos/Projects/xserver/composite/compwindow.c:454. A
    // window with no own redirect, no parent RedirectSubwindows,
    // gains a backing when reparented under a parent with active
    // RedirectSubwindows.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let unredirected_parent_xid = ResourceId(0x300_0001);
    let target_xid = ResourceId(0x300_0010);

    state
        .composite_redirects
        .redirect_subwindows(
            mate_panel_xid,
            state.resources.children(mate_panel_xid),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Automatic,
                owner: ClientId(14),
            },
        )
        .unwrap();

    seed_window(&mut state, mate_panel_xid, root_xid, 2560, 28);
    seed_window(&mut state, unredirected_parent_xid, root_xid, 100, 100);
    seed_window(&mut state, target_xid, unredirected_parent_xid, 50, 50);
    // Viewable throughout: an unviewable window gets its backing at realize instead.
    for w in [mate_panel_xid, unredirected_parent_xid, target_xid] {
        let _ = state.resources.map_window(w);
    }

    assert!(
        state
            .resources
            .window(target_xid)
            .unwrap()
            .redirected_backing
            .is_none(),
        "pre-condition: target has no backing"
    );

    dispatch_reparent_window(&mut state, &mut backend, target_xid, mate_panel_xid, 0, 0);

    assert_eq!(
        state.resources.window(target_xid).unwrap().parent,
        mate_panel_xid,
    );
    assert!(
        state
            .resources
            .window(target_xid)
            .unwrap()
            .redirected_backing
            .is_some(),
        "post-condition: target gained an inherited-redirect backing"
    );
}

#[test]
fn reparent_with_direct_redirect_keeps_backing() {
    // Phase 2 invariant: RedirectWindow(W) is a per-window
    // redirect independent of W's parent. Reparenting W must
    // NOT touch its backing.
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let mate_panel_xid = ResourceId(0x110_0003);
    let socket_xid = ResourceId(0x210_0013);
    let directly_redirected_xid = ResourceId(0x400_0001);

    state
        .composite_redirects
        .redirect_window(
            directly_redirected_xid,
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: ClientId(14),
            },
        )
        .unwrap();

    seed_window(&mut state, mate_panel_xid, root_xid, 2560, 28);
    seed_window(&mut state, socket_xid, mate_panel_xid, 26, 27);
    seed_window(&mut state, directly_redirected_xid, root_xid, 50, 50);
    seed_redirected_window(&mut state, &mut backend, directly_redirected_xid);

    let backing_before = state
        .resources
        .window(directly_redirected_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert!(backing_before.is_some());

    dispatch_reparent_window(
        &mut state,
        &mut backend,
        directly_redirected_xid,
        socket_xid,
        0,
        0,
    );

    let backing_after = state
        .resources
        .window(directly_redirected_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert_eq!(
        backing_before, backing_after,
        "RedirectWindow(W) survives reparent"
    );
}

// Window-storage step 4: a redirect backing follows viewability (Xorg
// compRealizeWindow / compUnrealizeWindow → compCheckRedirect); the
// redirect itself survives until Unredirect or destruction.

fn storage_req(
    state: &mut ServerState,
    backend: &mut crate::backend::recording::RecordingBackend,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request must succeed");
}

fn storage_redirect(
    state: &mut ServerState,
    backend: &mut crate::backend::recording::RecordingBackend,
    minor: u8,
    window: ResourceId,
) {
    let mut body = window.0.to_le_bytes().to_vec();
    body.extend_from_slice(&[1, 0, 0, 0]); // Manual
    storage_req(state, backend, 144, minor, &body);
}

fn storage_window_req(
    state: &mut ServerState,
    backend: &mut crate::backend::recording::RecordingBackend,
    opcode: u8,
    window: ResourceId,
) {
    storage_req(state, backend, opcode, 0, &window.0.to_le_bytes());
}

fn drain_calls(
    backend: &crate::backend::recording::RecordingBackend,
) -> Vec<crate::backend::recording::RecordedCall> {
    std::mem::take(&mut *backend.calls.lock().unwrap())
}

const MAP_WINDOW: u8 = 8;
const MAP_SUBWINDOWS: u8 = 9;
const UNMAP_WINDOW: u8 = 10;
const UNMAP_SUBWINDOWS: u8 = 11;

fn backing_of(state: &ServerState, window: ResourceId) -> Option<u32> {
    state
        .resources
        .window(window)
        .and_then(|w| w.redirected_backing.as_ref())
        .map(|b| b.host_pixmap.as_raw())
}

fn storage_host(window: ResourceId) -> u32 {
    0x8000_0000 | window.0
}

fn released(calls: &[crate::backend::recording::RecordedCall], backing: u32) -> bool {
    calls.iter().any(|c| {
        matches!(c, crate::backend::recording::RecordedCall::ReleaseRedirectedBacking(b) if *b == backing)
    })
}

fn allocations_for(calls: &[crate::backend::recording::RecordedCall], window: ResourceId) -> usize {
    calls
        .iter()
        .filter(|c| {
            matches!(c, crate::backend::recording::RecordedCall::AllocateRedirectedBacking { host_window, .. } if *host_window == storage_host(window))
        })
        .count()
}

fn participation_restored(
    calls: &[crate::backend::recording::RecordedCall],
    window: ResourceId,
) -> bool {
    calls.iter().any(|c| {
        matches!(c, crate::backend::recording::RecordedCall::SetWindowSceneParticipation { host_window, participating: true } if *host_window == storage_host(window))
    })
}

fn last_participation(
    calls: &[crate::backend::recording::RecordedCall],
    window: ResourceId,
) -> Option<bool> {
    calls.iter().rev().find_map(|c| match c {
        crate::backend::recording::RecordedCall::SetWindowSceneParticipation {
            host_window,
            participating,
        } if *host_window == storage_host(window) => Some(*participating),
        _ => None,
    })
}

/// A mapped top-level `W`, Manual-redirected through the dispatcher.
fn storage_redirected_top_level() -> (
    ServerState,
    crate::backend::recording::RecordingBackend,
    ResourceId,
    u32,
) {
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();
    let window = ResourceId(0x0600_0001);
    seed_window(&mut state, window, crate::resources::ROOT_WINDOW, 64, 48);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, window);
    storage_redirect(&mut state, &mut backend, 1, window);
    let backing = backing_of(&state, window).expect("viewable redirected window has a backing");
    (state, backend, window, backing)
}

#[test]
fn unmap_of_redirected_window_frees_backing_keeps_redirect_and_exclusion() {
    let (mut state, mut backend, window, backing) = storage_redirected_top_level();
    drain_calls(&backend);
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, window);
    let calls = drain_calls(&backend);
    assert!(
        released(&calls, backing),
        "unmap releases the backing; calls={calls:#?}"
    );
    assert_eq!(backing_of(&state, window), None);
    assert!(
        state.composite_redirects.window_mode(window).is_some(),
        "redirect record kept"
    );
    assert!(
        !participation_restored(&calls, window),
        "window stays out of the scene"
    );
}

#[test]
fn remap_of_redirected_window_allocates_a_fresh_backing() {
    let (mut state, mut backend, window, backing) = storage_redirected_top_level();
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, window);
    drain_calls(&backend);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, window);
    let calls = drain_calls(&backend);
    assert_eq!(allocations_for(&calls, window), 1);
    let fresh = backing_of(&state, window).expect("remap re-creates the backing");
    assert_ne!(fresh, backing);
    assert_eq!(
        last_participation(&calls, window),
        Some(false),
        "Manual stays excluded"
    );
}

#[test]
fn redirect_of_unviewable_window_allocates_at_map() {
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();
    let window = ResourceId(0x0600_0001);
    seed_window(&mut state, window, crate::resources::ROOT_WINDOW, 64, 48);
    storage_redirect(&mut state, &mut backend, 1, window);
    assert_eq!(
        allocations_for(&drain_calls(&backend), window),
        0,
        "Xorg: not realized, no pixmap"
    );
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, window);
    assert_eq!(allocations_for(&drain_calls(&backend), window), 1);
    assert!(backing_of(&state, window).is_some());
}

#[test]
fn ancestor_unmap_frees_redirected_descendant_backing_and_remap_restores_it() {
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();
    let frame = ResourceId(0x0600_0010);
    let window = ResourceId(0x0600_0011);
    seed_window(&mut state, frame, crate::resources::ROOT_WINDOW, 64, 48);
    seed_window(&mut state, window, frame, 32, 24);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, window);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, frame);
    storage_redirect(&mut state, &mut backend, 1, window);
    let backing = backing_of(&state, window).expect("backing");
    drain_calls(&backend);

    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, frame);
    let calls = drain_calls(&backend);
    assert!(released(&calls, backing));
    assert_eq!(backing_of(&state, window), None);
    assert!(state.composite_redirects.window_mode(window).is_some());
    assert!(!participation_restored(&calls, window));

    storage_window_req(&mut state, &mut backend, MAP_WINDOW, frame);
    let calls = drain_calls(&backend);
    assert_eq!(
        allocations_for(&calls, window),
        1,
        "descendant realized with its ancestor"
    );
    assert_ne!(backing_of(&state, window), Some(backing));
    assert!(backing_of(&state, window).is_some());
    assert_eq!(last_participation(&calls, window), Some(false));
}

#[test]
fn redirect_subwindows_children_follow_viewability() {
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();
    let parent = ResourceId(0x0600_0020);
    let shown = ResourceId(0x0600_0021);
    let hidden = ResourceId(0x0600_0022);
    seed_window(&mut state, parent, crate::resources::ROOT_WINDOW, 64, 48);
    seed_window(&mut state, shown, parent, 16, 16);
    seed_window(&mut state, hidden, parent, 16, 16);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, parent);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, shown);
    storage_redirect(&mut state, &mut backend, 2, parent); // RedirectSubwindows
    let shown_backing = backing_of(&state, shown).expect("viewable child redirected");
    assert_eq!(
        backing_of(&state, hidden),
        None,
        "unmapped child has no backing"
    );
    assert_eq!(
        backing_of(&state, parent),
        None,
        "the parent itself is not redirected"
    );

    storage_window_req(&mut state, &mut backend, UNMAP_SUBWINDOWS, parent);
    let calls = drain_calls(&backend);
    assert!(released(&calls, shown_backing));
    assert_eq!(backing_of(&state, shown), None);
    assert!(state.composite_redirects.subwindows_mode(parent).is_some());
    assert!(!participation_restored(&calls, shown));

    storage_window_req(&mut state, &mut backend, MAP_SUBWINDOWS, parent);
    let calls = drain_calls(&backend);
    assert_eq!(allocations_for(&calls, shown), 1);
    assert_eq!(allocations_for(&calls, hidden), 1);
    assert_eq!(last_participation(&calls, shown), Some(false));
    assert_eq!(last_participation(&calls, hidden), Some(false));

    let (a, b) = (
        backing_of(&state, shown).unwrap(),
        backing_of(&state, hidden).unwrap(),
    );
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, parent);
    let calls = drain_calls(&backend);
    assert!(
        released(&calls, a) && released(&calls, b),
        "ancestor unmap frees every child"
    );
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, parent);
    let calls = drain_calls(&backend);
    assert_eq!(allocations_for(&calls, shown), 1);
    assert_eq!(allocations_for(&calls, hidden), 1);
}

#[test]
fn unredirect_window_still_tears_down_and_restores_participation() {
    let (mut state, mut backend, window, backing) = storage_redirected_top_level();
    drain_calls(&backend);
    storage_redirect(&mut state, &mut backend, 3, window); // UnredirectWindow
    let calls = drain_calls(&backend);
    assert!(released(&calls, backing));
    assert_eq!(backing_of(&state, window), None);
    assert!(state.composite_redirects.window_mode(window).is_none());
    assert_eq!(last_participation(&calls, window), Some(true));
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, window);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, window);
    assert_eq!(
        allocations_for(&drain_calls(&backend), window),
        0,
        "no redirect, no backing"
    );
}

#[test]
fn unredirect_of_hidden_mapped_window_restores_participation_without_backing() {
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();
    let frame = ResourceId(0x0600_0030);
    let window = ResourceId(0x0600_0031);
    seed_window(&mut state, frame, crate::resources::ROOT_WINDOW, 64, 48);
    seed_window(&mut state, window, frame, 32, 24);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, window);
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, frame);
    storage_redirect(&mut state, &mut backend, 1, window);
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, frame);
    drain_calls(&backend);
    storage_redirect(&mut state, &mut backend, 3, window);
    let calls = drain_calls(&backend);
    assert_eq!(
        last_participation(&calls, window),
        Some(true),
        "mapped window rejoins the scene"
    );
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, frame);
    assert_eq!(allocations_for(&drain_calls(&backend), window), 0);
}

#[test]
fn destroy_of_redirected_window_still_releases_backing_and_record() {
    let (mut state, mut backend, window, backing) = storage_redirected_top_level();
    drain_calls(&backend);
    storage_window_req(&mut state, &mut backend, 4, window); // DestroyWindow
    let calls = drain_calls(&backend);
    assert!(released(&calls, backing));
    assert!(state.composite_redirects.window_mode(window).is_none());
}

#[test]
fn destroy_of_unmapped_redirected_window_drops_the_record() {
    let (mut state, mut backend, window, _backing) = storage_redirected_top_level();
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, window);
    storage_window_req(&mut state, &mut backend, 4, window);
    assert!(state.composite_redirects.window_mode(window).is_none());
}

#[test]
fn reparent_under_unmapped_parent_frees_backing_keeps_redirect() {
    let (mut state, mut backend, window, backing) = storage_redirected_top_level();
    let hidden_parent = ResourceId(0x0600_0040);
    seed_window(
        &mut state,
        hidden_parent,
        crate::resources::ROOT_WINDOW,
        64,
        48,
    );
    drain_calls(&backend);
    dispatch_reparent_window(&mut state, &mut backend, window, hidden_parent, 0, 0);
    assert!(released(&drain_calls(&backend), backing));
    assert_eq!(backing_of(&state, window), None);
    assert!(state.composite_redirects.window_mode(window).is_some());
    storage_window_req(&mut state, &mut backend, MAP_WINDOW, hidden_parent);
    assert_eq!(allocations_for(&drain_calls(&backend), window), 1);
}

#[test]
fn unmap_severs_named_pixmaps_from_the_window() {
    let (mut state, mut backend, window, backing) = storage_redirected_top_level();
    let named = ResourceId(0x0600_0050);
    if let Some(w) = state.resources.window_mut(window) {
        w.composite_named_pixmaps
            .push(crate::resources::NamedCompositePixmap {
                client_pixmap: named,
                host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(backing),
                width: 64,
                height: 48,
            });
    }
    storage_window_req(&mut state, &mut backend, UNMAP_WINDOW, window);
    assert!(
        state
            .resources
            .window(window)
            .unwrap()
            .composite_named_pixmaps
            .is_empty()
    );
    assert_eq!(
        render_picture_damage_drawable(&state, named),
        named,
        "no longer the window's"
    );
}

#[test]
fn reparent_between_redirected_parents_with_different_modes_flips_mode() {
    // Phase 2: when both old_parent and new_parent have
    // RedirectSubwindows but with different modes (Manual ↔
    // Automatic), the production reconciliation calls
    // flip_redirect_target_mode. That helper preserves the
    // backing handle (X Composite spec: old named pixmaps remain
    // allocated until FreePixmap; only redirectDraw flips) and
    // updates the window's own scene_participating flag per the
    // new mode (Manual ⇒ false, Automatic ⇒ true).
    let mut state = make_test_state();
    let mut backend = crate::backend::recording::RecordingBackend::new().with_redirect_activation();

    let root_xid = crate::resources::ROOT_WINDOW;
    let parent_manual_xid = ResourceId(0x500_0001);
    let parent_automatic_xid = ResourceId(0x500_0002);
    let target_xid = ResourceId(0x500_0010);

    state
        .composite_redirects
        .redirect_subwindows(
            parent_manual_xid,
            state.resources.children(parent_manual_xid),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Manual,
                owner: ClientId(14),
            },
        )
        .unwrap();
    state
        .composite_redirects
        .redirect_subwindows(
            parent_automatic_xid,
            state.resources.children(parent_automatic_xid),
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Automatic,
                owner: ClientId(14),
            },
        )
        .unwrap();

    seed_window(&mut state, parent_manual_xid, root_xid, 100, 100);
    seed_window(&mut state, parent_automatic_xid, root_xid, 100, 100);
    // target is initially a child of parent_manual_xid → inherits
    // Manual redirect → has a backing allocated, and its own
    // scene_participating is false (Manual semantics).
    seed_window(&mut state, target_xid, parent_manual_xid, 50, 50);
    seed_redirected_window(&mut state, &mut backend, target_xid);

    let backing_before = state
        .resources
        .window(target_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert!(
        backing_before.is_some(),
        "pre: target has Manual-inherited backing"
    );

    // Reparent to the Automatic parent.
    dispatch_reparent_window(
        &mut state,
        &mut backend,
        target_xid,
        parent_automatic_xid,
        0,
        0,
    );

    let backing_after = state
        .resources
        .window(target_xid)
        .unwrap()
        .redirected_backing
        .as_ref()
        .map(|b| b.host_pixmap);
    assert_eq!(
        backing_before, backing_after,
        "mode-flip across redirected parents preserves the backing handle (X Composite spec)"
    );

    // Mode flip side effect: scene_participating flag on the
    // window itself follows the new parent's mode. The flip is
    // visible in the RecordingBackend's call log — look for a
    // SetWindowSceneParticipation(_, true) for target's host xid
    // (Automatic ⇒ participating=true; pre-reparent it was
    // false under Manual).
    let calls = backend.calls.lock().expect("calls poisoned");
    let saw_participation_true = calls.iter().any(|c| {
        matches!(
            c,
            crate::backend::recording::RecordedCall::SetWindowSceneParticipation {
                participating: true,
                ..
            }
        )
    });
    assert!(
        saw_participation_true,
        "expected SetWindowSceneParticipation(_, true) after Manual → Automatic flip; \
             got {:?}",
        *calls,
    );
}
