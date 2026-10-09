use super::*;

/// How a TFP compositor holds the named backing of a window that is then resized.
#[derive(Clone, Copy, Debug)]
enum TfpHold {
    NameOnly,
    Glx,
    GlxAndDri3,
    Dri3Only,
}

/// How that compositor lets go of it.
#[derive(Clone, Copy, Debug)]
enum TfpTeardown {
    DestroyGlxThenFree,
    FreeThenDestroyGlx,
    DestroyWindowFirst,
    Disconnect,
}

/// Every backing the window ever had is gone: no export entry, alias hold or store entry.
fn assert_backings_released(f: &ProtoFixture, backings: &[u32], what: &str) {
    for &b in backings {
        assert!(
            !f.backend.has_export_entry(b),
            "{what}: export entry on 0x{b:x} leaked"
        );
        assert!(
            f.backend.test_alias_registry_get(b).is_none(),
            "{what}: alias hold on 0x{b:x} leaked: {:?}",
            f.backend.test_alias_registry_get(b),
        );
        assert!(
            !f.backend.store_drawable_exists_for_tests(b),
            "{what}: store entry for 0x{b:x} leaked"
        );
    }
}

/// picom-glx: NameWindowPixmap + glXCreatePixmap, then resizes rotate the backing.
/// The GLX pixmap is retargeted onto each new backing (110f1a90); its export ref must follow.
/// Client 1 is the compositor, client 2 owns the window.
fn tfp_resize_scenario(hold: TfpHold, resizes: i32, teardown: TfpTeardown) -> bool {
    const APP: u32 = 2;
    const WIN: u32 = 0x0078_0001;
    const PIX: u32 = 0x0078_0002;
    const GLXPIX: u32 = 0x0078_0003;
    let what = format!("{hold:?} resizes={resizes} {teardown:?}");
    let Some(mut f) = ProtoFixture::new() else {
        return false;
    };
    let _app_peer = f.add_client(APP);
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let mut cw = Vec::new();
    cw.extend_from_slice(&WIN.to_le_bytes());
    cw.extend_from_slice(&root.to_le_bytes());
    cw.extend_from_slice(&[0, 0, 0, 0]); // x, y
    cw.extend_from_slice(&100u16.to_le_bytes());
    cw.extend_from_slice(&50u16.to_le_bytes());
    cw.extend_from_slice(&0u16.to_le_bytes()); // border
    cw.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
    cw.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
    cw.extend_from_slice(&0u32.to_le_bytes()); // no values
    f.req_as(APP, 1, 24, &cw);
    let mut redirect = root.to_le_bytes().to_vec();
    redirect.extend_from_slice(&[1, 0, 0, 0]); // CompositeRedirectManual
    f.req(144, 2, &redirect);
    f.req_as(APP, 8, 0, &WIN.to_le_bytes()); // MapWindow
    let mut name = WIN.to_le_bytes().to_vec();
    name.extend_from_slice(&PIX.to_le_bytes());
    f.req(144, 6, &name); // NameWindowPixmap
    let named = |f: &ProtoFixture| {
        f.state
            .resources
            .pixmap(yserver_protocol::x11::ResourceId(PIX))
            .and_then(|p| p.host_xid)
            .map(|h| h.as_raw())
            .expect("named pixmap")
    };
    let mut backings = vec![named(&f)];
    let glx = matches!(hold, TfpHold::Glx | TfpHold::GlxAndDri3);
    if glx {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // screen
        body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
        body.extend_from_slice(&PIX.to_le_bytes());
        body.extend_from_slice(&GLXPIX.to_le_bytes());
        f.req(148, yserver_protocol::x11::glx::CREATE_PIXMAP, &body);
    }
    if matches!(hold, TfpHold::GlxAndDri3 | TfpHold::Dri3Only) {
        match f.backend.dri3_export_pixmap_buffers(backings[0]) {
            Ok(export) => drop(export.fd), // the client's own fd; ours is the entry's dup
            Err(err) => {
                eprintln!("{what}: skipping, this ICD cannot export the backing: {err}");
                return true;
            }
        }
    }
    assert_eq!(
        f.backend.has_export_entry(backings[0]),
        !matches!(hold, TfpHold::NameOnly),
        "{what}: export entry on the named backing"
    );
    for i in 1..=resizes {
        let mut cfg = WIN.to_le_bytes().to_vec();
        cfg.extend_from_slice(&0x0cu16.to_le_bytes());
        cfg.extend_from_slice(&0u16.to_le_bytes());
        cfg.extend_from_slice(&(100 + 10 * i).to_le_bytes());
        cfg.extend_from_slice(&(50 + 10 * i).to_le_bytes());
        f.req_as(APP, 12, 0, &cfg);
        let current = named(&f);
        assert!(!backings.contains(&current), "{what}: resize {i} rotated");
        backings.push(current);
    }
    let destroy_glx = |f: &mut ProtoFixture| {
        if glx {
            f.req(
                148,
                yserver_protocol::x11::glx::DESTROY_PIXMAP,
                &GLXPIX.to_le_bytes(),
            );
        }
    };
    let free = |f: &mut ProtoFixture| f.req(54, 0, &PIX.to_le_bytes());
    let destroy_win = |f: &mut ProtoFixture| f.req_as(APP, 4, 0, &WIN.to_le_bytes());
    match teardown {
        TfpTeardown::DestroyGlxThenFree => {
            destroy_glx(&mut f);
            free(&mut f);
            destroy_win(&mut f);
        }
        TfpTeardown::FreeThenDestroyGlx => {
            free(&mut f);
            destroy_glx(&mut f);
            destroy_win(&mut f);
        }
        TfpTeardown::DestroyWindowFirst => {
            destroy_win(&mut f);
            destroy_glx(&mut f);
            free(&mut f);
        }
        TfpTeardown::Disconnect => {
            yserver_core::core_loop::process_disconnect::process_disconnect(
                &mut f.state,
                &mut f.backend,
                yserver_protocol::x11::ClientId(1),
            );
            destroy_win(&mut f);
        }
    }
    for _ in 0..200 {
        f.backend.for_tests_poll_retired();
        if backings
            .iter()
            .all(|&b| !f.backend.store_drawable_exists_for_tests(b))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_backings_released(&f, &backings, &what);
    true
}

/// A resize must not strand the pre-resize backing behind a GLX/DRI3 export ref.
/// The DRI3 holds need an exporting ICD (RADV); lavapipe cannot export and skips them.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_resized_tfp_backing_is_freed_once_the_compositor_lets_go() {
    for hold in [
        TfpHold::NameOnly,
        TfpHold::Glx,
        TfpHold::GlxAndDri3,
        TfpHold::Dri3Only,
    ] {
        for resizes in [0, 1, 2] {
            for teardown in [
                TfpTeardown::DestroyGlxThenFree,
                TfpTeardown::FreeThenDestroyGlx,
                TfpTeardown::DestroyWindowFirst,
                TfpTeardown::Disconnect,
            ] {
                if !tfp_resize_scenario(hold, resizes, teardown) {
                    eprintln!("skipping: no Vk");
                    return;
                }
            }
        }
    }
}

/// Who ends a redirected toplevel's life, and how.
#[derive(Clone, Copy, Debug)]
enum RedirectTeardown {
    AppDestroyWindow,
    AppDisconnect,
    CompositorDisconnect,
    CompositorThenAppDisconnect,
}

/// picom's RedirectSubwindows(root, Manual) over an app toplevel with a child; client 1 is the
/// compositor, client 2 the app. Every teardown must release the toplevel's redirect backing.
fn redirect_teardown_scenario(named: bool, teardown: RedirectTeardown) -> bool {
    use yserver_protocol::x11::{ClientId, ResourceId};
    const COMP: u32 = 1;
    const APP: u32 = 2;
    const WIN: u32 = 0x0079_0001;
    const CHILD: u32 = 0x0079_0002;
    const PIX: u32 = 0x0079_0003;
    let what = format!("named={named} {teardown:?}");
    let Some(mut f) = ProtoFixture::new() else {
        return false;
    };
    let _app_peer = f.add_client(APP);
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let create = |f: &mut ProtoFixture, wid: u32, parent: u32, w: u16, h: u16| {
        let mut cw = Vec::new();
        cw.extend_from_slice(&wid.to_le_bytes());
        cw.extend_from_slice(&parent.to_le_bytes());
        cw.extend_from_slice(&[0, 0, 0, 0]); // x, y
        cw.extend_from_slice(&w.to_le_bytes());
        cw.extend_from_slice(&h.to_le_bytes());
        cw.extend_from_slice(&0u16.to_le_bytes()); // border
        cw.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
        cw.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
        cw.extend_from_slice(&0u32.to_le_bytes()); // no values
        f.req_as(APP, 1, 24, &cw);
    };
    let mut redirect = root.to_le_bytes().to_vec();
    redirect.extend_from_slice(&[1, 0, 0, 0]); // CompositeRedirectManual
    f.req_as(COMP, 144, 2, &redirect);
    create(&mut f, WIN, root, 100, 50);
    create(&mut f, CHILD, WIN, 20, 10);
    f.req_as(APP, 8, 0, &CHILD.to_le_bytes()); // MapWindow
    f.req_as(APP, 8, 0, &WIN.to_le_bytes());
    let backing = f
        .state
        .resources
        .window(ResourceId(WIN))
        .and_then(|w| w.redirected_backing.as_ref())
        .map(|b| b.host_pixmap.as_raw())
        .expect("mapped redirected toplevel has a backing");
    if named {
        let mut name = WIN.to_le_bytes().to_vec();
        name.extend_from_slice(&PIX.to_le_bytes());
        f.req_as(COMP, 144, 6, &name); // NameWindowPixmap
    }
    let disconnect = |f: &mut ProtoFixture, client: u32| {
        yserver_core::core_loop::process_disconnect::process_disconnect(
            &mut f.state,
            &mut f.backend,
            ClientId(client),
        );
    };
    let assert_held = |f: &ProtoFixture| {
        assert!(
            f.backend.store_drawable_exists_for_tests(backing),
            "{what}: the named pixmap still holds the backing"
        );
    };
    match teardown {
        RedirectTeardown::AppDestroyWindow | RedirectTeardown::AppDisconnect => {
            if matches!(teardown, RedirectTeardown::AppDestroyWindow) {
                f.req_as(APP, 4, 0, &WIN.to_le_bytes()); // DestroyWindow
            } else {
                disconnect(&mut f, APP);
            }
            if named {
                assert_held(&f);
                f.req_as(COMP, 54, 0, &PIX.to_le_bytes()); // FreePixmap
            }
        }
        RedirectTeardown::CompositorDisconnect | RedirectTeardown::CompositorThenAppDisconnect => {
            disconnect(&mut f, COMP);
            assert!(
                f.state.composite_redirects.is_empty(),
                "{what}: the compositor's redirect survived it"
            );
            let win = f.state.resources.window(ResourceId(WIN));
            assert!(
                win.is_some_and(|w| w.redirected_backing.is_none()),
                "{what}: the app's window must survive, unredirected"
            );
            if matches!(teardown, RedirectTeardown::CompositorThenAppDisconnect) {
                disconnect(&mut f, APP);
            }
        }
    }
    for _ in 0..200 {
        f.backend.for_tests_poll_retired();
        if !f.backend.store_drawable_exists_for_tests(backing) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_backings_released(&f, &[backing], &what);
    true
}

/// A redirected toplevel's backing is released however its app or its compositor goes away.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_redirect_backing_is_released_when_its_app_or_compositor_disconnects() {
    for named in [false, true] {
        for teardown in [
            RedirectTeardown::AppDestroyWindow,
            RedirectTeardown::AppDisconnect,
            RedirectTeardown::CompositorDisconnect,
            RedirectTeardown::CompositorThenAppDisconnect,
        ] {
            if !redirect_teardown_scenario(named, teardown) {
                eprintln!("skipping: no Vk");
                return;
            }
        }
    }
}

/// The export-holders report shows a named GLX-bound redirect backing until it is released.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn export_holders_report_tracks_a_named_glx_backing_until_release() {
    use yserver_core::backend::export_holders::collect_core_holders;
    const APP: u32 = 2;
    const WIN: u32 = 0x007a_0001;
    const PIX: u32 = 0x007a_0002;
    const GLXPIX: u32 = 0x007a_0003;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let _app_peer = f.add_client(APP);
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let mut cw = Vec::new();
    cw.extend_from_slice(&WIN.to_le_bytes());
    cw.extend_from_slice(&root.to_le_bytes());
    cw.extend_from_slice(&[0, 0, 0, 0]); // x, y
    cw.extend_from_slice(&100u16.to_le_bytes());
    cw.extend_from_slice(&50u16.to_le_bytes());
    cw.extend_from_slice(&0u16.to_le_bytes()); // border
    cw.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
    cw.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
    cw.extend_from_slice(&0u32.to_le_bytes()); // no values
    f.req_as(APP, 1, 24, &cw);
    let mut redirect = root.to_le_bytes().to_vec();
    redirect.extend_from_slice(&[1, 0, 0, 0]); // CompositeRedirectManual
    f.req(144, 2, &redirect);
    f.req_as(APP, 8, 0, &WIN.to_le_bytes()); // MapWindow
    let mut name = WIN.to_le_bytes().to_vec();
    name.extend_from_slice(&PIX.to_le_bytes());
    f.req(144, 6, &name); // NameWindowPixmap
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes()); // screen
    body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
    body.extend_from_slice(&PIX.to_le_bytes());
    body.extend_from_slice(&GLXPIX.to_le_bytes());
    f.req(148, yserver_protocol::x11::glx::CREATE_PIXMAP, &body);
    let backing = f
        .state
        .resources
        .pixmap(yserver_protocol::x11::ResourceId(PIX))
        .and_then(|p| p.host_xid)
        .map(|h| h.as_raw())
        .expect("named pixmap");
    let report = f
        .backend
        .export_holders_report_for_tests(&collect_core_holders(&f.state));
    eprintln!("{}", report.join("\n"));
    let line = report
        .iter()
        .find(|l| l.starts_with(&format!("  0x{backing:x} ")))
        .unwrap_or_else(|| panic!("backing 0x{backing:x} missing from {report:#?}"));
    // Redirect hold + named alias + the GLX export's lifetime ref.
    assert!(line.contains(" alias_rc=3 "), "{line}");
    assert!(
        line.contains(" export=[glx_refs=1 dri3_fd=n lifetime=alias] sync_dup=n "),
        "{line}"
    );
    assert!(line.contains(" xid=attached "), "{line}");
    assert!(line.contains(" redirect_of=0x"), "{line}");
    assert!(
        line.contains(&format!(
            "c1:named 0x{PIX:x}(win 0x{WIN:x}) c1:glxpixmap 0x{GLXPIX:x}(of 0x{PIX:x}) \
             c2:redirect-of win 0x{WIN:x}]"
        )),
        "{line}"
    );
    assert!(
        f.backend
            .report_export_holders(&|| collect_core_holders(&f.state))
    );
    assert!(
        !f.backend
            .report_export_holders(&|| collect_core_holders(&f.state))
    );

    f.req(
        148,
        yserver_protocol::x11::glx::DESTROY_PIXMAP,
        &GLXPIX.to_le_bytes(),
    );
    f.req(54, 0, &PIX.to_le_bytes()); // FreePixmap
    f.req_as(APP, 4, 0, &WIN.to_le_bytes()); // DestroyWindow
    for _ in 0..200 {
        f.backend.for_tests_poll_retired();
        if !f.backend.store_drawable_exists_for_tests(backing) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let report = f
        .backend
        .export_holders_report_for_tests(&collect_core_holders(&f.state));
    assert!(
        !report
            .iter()
            .any(|l| l.starts_with(&format!("  0x{backing:x} "))),
        "{report:#?}"
    );
    assert!(
        f.backend
            .report_export_holders(&|| collect_core_holders(&f.state))
    );
}

/// How a compositor overlaps its `NameWindowPixmap` names on one backing.
#[derive(Clone, Copy, Debug)]
enum NameOverlap {
    TwoNamesFreedInOrder,
    HundredRenames,
    GlxOnEachName,
    NamesOutliveWindow,
    CompositorDisconnect,
    NameAsBackground,
    RenameWhileBackgroundDeferred,
    ResizeAfterAFreedName,
}

/// Each name owns one alias ref (Xorg `compext.c:260`); each FreePixmap drops one (`dispatch.c:1540`).
fn name_overlap_scenario(overlap: NameOverlap) -> bool {
    use yserver_core::backend::export_holders::collect_core_holders;
    use yserver_protocol::x11::{ClientId, ResourceId};
    const COMP: u32 = 1;
    const APP: u32 = 2;
    const WIN: u32 = 0x007b_0001;
    const BG_WIN: u32 = 0x007b_0002;
    const PIX0: u32 = 0x007c_0000;
    const GLX0: u32 = 0x007d_0000;
    let what = format!("{overlap:?}");
    let Some(mut f) = ProtoFixture::new() else {
        return false;
    };
    let _app_peer = f.add_client(APP);
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let create = |f: &mut ProtoFixture, wid: u32| {
        let mut cw = Vec::new();
        cw.extend_from_slice(&wid.to_le_bytes());
        cw.extend_from_slice(&root.to_le_bytes());
        cw.extend_from_slice(&[0, 0, 0, 0]); // x, y
        cw.extend_from_slice(&100u16.to_le_bytes());
        cw.extend_from_slice(&50u16.to_le_bytes());
        cw.extend_from_slice(&0u16.to_le_bytes()); // border
        cw.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
        cw.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
        cw.extend_from_slice(&0u32.to_le_bytes()); // no values
        f.req_as(APP, 1, 24, &cw);
    };
    let mut redirect = root.to_le_bytes().to_vec();
    redirect.extend_from_slice(&[1, 0, 0, 0]); // CompositeRedirectManual
    f.req_as(COMP, 144, 2, &redirect);
    create(&mut f, WIN);
    create(&mut f, BG_WIN); // left unmapped: no backing of its own
    f.req_as(APP, 8, 0, &WIN.to_le_bytes()); // MapWindow
    let backing = f
        .state
        .resources
        .window(ResourceId(WIN))
        .and_then(|w| w.redirected_backing.as_ref())
        .map(|b| b.host_pixmap.as_raw())
        .expect("mapped redirected toplevel has a backing");
    let name = |f: &mut ProtoFixture, i: u32| {
        let mut body = WIN.to_le_bytes().to_vec();
        body.extend_from_slice(&(PIX0 + i).to_le_bytes());
        f.req_as(COMP, 144, 6, &body); // NameWindowPixmap
        let host = f
            .state
            .resources
            .pixmap(ResourceId(PIX0 + i))
            .and_then(|p| p.host_xid)
            .map(|h| h.as_raw());
        assert!(host.is_some(), "name {i} aliases a backing");
    };
    let free = |f: &mut ProtoFixture, i: u32| f.req_as(COMP, 54, 0, &(PIX0 + i).to_le_bytes());
    let glx_create = |f: &mut ProtoFixture, i: u32| {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // screen
        body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
        body.extend_from_slice(&(PIX0 + i).to_le_bytes());
        body.extend_from_slice(&(GLX0 + i).to_le_bytes());
        f.req_as(COMP, 148, yserver_protocol::x11::glx::CREATE_PIXMAP, &body);
    };
    let glx_destroy = |f: &mut ProtoFixture, i: u32| {
        f.req_as(
            COMP,
            148,
            yserver_protocol::x11::glx::DESTROY_PIXMAP,
            &(GLX0 + i).to_le_bytes(),
        );
    };
    let set_bg = |f: &mut ProtoFixture, pixmap: u32| {
        let mut body = BG_WIN.to_le_bytes().to_vec();
        body.extend_from_slice(&1u32.to_le_bytes()); // CWBackPixmap
        body.extend_from_slice(&pixmap.to_le_bytes());
        f.req_as(APP, 2, 0, &body); // ChangeWindowAttributes
    };
    let destroy_win = |f: &mut ProtoFixture| f.req_as(APP, 4, 0, &WIN.to_le_bytes());
    let mut backings = vec![backing];
    let settle = |f: &mut ProtoFixture, backings: &[u32]| {
        for _ in 0..200 {
            f.backend.for_tests_poll_retired();
            if backings
                .iter()
                .all(|&b| !f.backend.store_drawable_exists_for_tests(b))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let assert_held = |f: &ProtoFixture, why: &str| {
        assert!(
            f.backend.store_drawable_exists_for_tests(backing),
            "{what}: {why} must keep the backing alive"
        );
    };
    match overlap {
        NameOverlap::TwoNamesFreedInOrder => {
            name(&mut f, 1);
            name(&mut f, 2);
            free(&mut f, 1);
            free(&mut f, 2);
            destroy_win(&mut f);
        }
        NameOverlap::HundredRenames => {
            name(&mut f, 0);
            for i in 1..100 {
                name(&mut f, i);
                free(&mut f, i - 1);
            }
            free(&mut f, 99);
            destroy_win(&mut f);
        }
        NameOverlap::GlxOnEachName => {
            name(&mut f, 0);
            glx_create(&mut f, 0);
            for i in 1..5 {
                name(&mut f, i);
                glx_create(&mut f, i);
                glx_destroy(&mut f, i - 1);
                free(&mut f, i - 1);
            }
            glx_destroy(&mut f, 4);
            free(&mut f, 4);
            destroy_win(&mut f);
        }
        NameOverlap::NamesOutliveWindow => {
            for i in 0..3 {
                name(&mut f, i);
            }
            destroy_win(&mut f);
            assert_held(&f, "outstanding names");
            for i in 0..3 {
                free(&mut f, i);
            }
        }
        NameOverlap::CompositorDisconnect => {
            for i in 0..3 {
                name(&mut f, i);
            }
            glx_create(&mut f, 2);
            yserver_core::core_loop::process_disconnect::process_disconnect(
                &mut f.state,
                &mut f.backend,
                ClientId(COMP),
            );
            destroy_win(&mut f);
        }
        NameOverlap::NameAsBackground => {
            name(&mut f, 1);
            name(&mut f, 2);
            set_bg(&mut f, PIX0 + 2);
            free(&mut f, 1);
            free(&mut f, 2);
            destroy_win(&mut f);
            settle(&mut f, &backings);
            assert_held(&f, "a window background naming it");
            set_bg(&mut f, 0); // None
        }
        NameOverlap::RenameWhileBackgroundDeferred => {
            name(&mut f, 1);
            set_bg(&mut f, PIX0 + 1);
            free(&mut f, 1);
            name(&mut f, 2);
            free(&mut f, 2);
            destroy_win(&mut f);
            settle(&mut f, &backings);
            assert_held(&f, "a window background naming it");
            set_bg(&mut f, 0); // None
        }
        NameOverlap::ResizeAfterAFreedName => {
            name(&mut f, 1);
            name(&mut f, 2);
            free(&mut f, 1);
            let mut cfg = WIN.to_le_bytes().to_vec();
            cfg.extend_from_slice(&0x0cu16.to_le_bytes()); // width | height
            cfg.extend_from_slice(&0u16.to_le_bytes());
            cfg.extend_from_slice(&120u32.to_le_bytes());
            cfg.extend_from_slice(&70u32.to_le_bytes());
            f.req_as(APP, 12, 0, &cfg); // ConfigureWindow
            let rotated = f
                .state
                .resources
                .pixmap(ResourceId(PIX0 + 2))
                .and_then(|p| p.host_xid)
                .map(|h| h.as_raw())
                .expect("live name");
            assert_ne!(rotated, backing, "{what}: resize rotated the backing");
            backings.push(rotated);
            free(&mut f, 2);
            destroy_win(&mut f);
        }
    }
    settle(&mut f, &backings);
    assert_backings_released(&f, &backings, &what);
    let report = f
        .backend
        .export_holders_report_for_tests(&collect_core_holders(&f.state));
    for b in &backings {
        assert!(
            !report.iter().any(|l| l.starts_with(&format!("  0x{b:x} "))),
            "{what}: {report:#?}"
        );
    }
    true
}

/// Name twice, free both, destroy: the backing goes.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn two_overlapping_names_freed_in_order_release_the_backing() {
    if !name_overlap_scenario(NameOverlap::TwoNamesFreedInOrder) {
        eprintln!("skipping: no Vk");
    }
}

/// picom's rename-before-free, 100 times (hardware saw alias_rc=172).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_hundred_overlapping_renames_release_the_backing() {
    if !name_overlap_scenario(NameOverlap::HundredRenames) {
        eprintln!("skipping: no Vk");
    }
}

/// picom glx: every name carries its own GLXPixmap.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn overlapping_names_each_with_a_glx_pixmap_release_the_backing() {
    if !name_overlap_scenario(NameOverlap::GlxOnEachName) {
        eprintln!("skipping: no Vk");
    }
}

/// The window dies first; its names keep the backing until freed.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn overlapping_names_that_outlive_the_window_release_the_backing() {
    if !name_overlap_scenario(NameOverlap::NamesOutliveWindow) {
        eprintln!("skipping: no Vk");
    }
}

/// The compositor exits with several names outstanding.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_compositor_disconnect_releases_every_outstanding_name() {
    if !name_overlap_scenario(NameOverlap::CompositorDisconnect) {
        eprintln!("skipping: no Vk");
    }
}

/// A freed name that is still a window background defers until it is replaced.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_name_used_as_a_background_is_held_until_the_background_changes() {
    if !name_overlap_scenario(NameOverlap::NameAsBackground) {
        eprintln!("skipping: no Vk");
    }
}

/// A resize retargets only live names; a freed one must not move a ref onto the new backing.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_resize_after_a_freed_name_releases_both_backings() {
    if !name_overlap_scenario(NameOverlap::ResizeAfterAFreedName) {
        eprintln!("skipping: no Vk");
    }
}

/// A second name freed while a background holds the first deferred ref.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_rename_while_a_background_holds_the_backing_does_not_leak() {
    if !name_overlap_scenario(NameOverlap::RenameWhileBackgroundDeferred) {
        eprintln!("skipping: no Vk");
    }
}
