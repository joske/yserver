use super::*;

// #96: each visual-backed GLX FBConfig must advertise GLX_PBUFFER_BIT plus
// the three GLX_MAX_PBUFFER_* caps, or Chromium/ANGLE can't allocate its
// offscreen pbuffer surface and falls back to software (no WebGL/Maps 3D).
// The #152 native-pixmap config is deliberately pixmap-only.  Property
// counts must stay uniform across configs (GetFBConfigs encodes a single
// num_properties for all of them).
#[test]
fn glx_fb_configs_advertise_pbuffer() {
    use yserver_protocol::x11::glx as g;
    for tfp in [false, true] {
        let configs = synthesise_glx_fb_configs(tfp);
        assert!(!configs.is_empty());
        let prop_count = configs[0].len();
        for config in &configs {
            assert_eq!(config.len(), prop_count, "non-uniform property count");
            let get = |attr: u32| config.iter().find(|(a, _)| *a == attr).map(|(_, v)| *v);
            let drawable = get(g::GLX_DRAWABLE_TYPE).expect("DRAWABLE_TYPE present");
            if drawable == g::GLX_PIXMAP_BIT {
                assert_eq!(get(g::GLX_VISUAL_ID), Some(0));
                assert_eq!(get(g::GLX_X_RENDERABLE), Some(0));
                continue;
            }
            assert_ne!(
                drawable & g::GLX_PBUFFER_BIT,
                0,
                "config must advertise GLX_PBUFFER_BIT (tfp={tfp})"
            );
            let max_w = get(g::GLX_MAX_PBUFFER_WIDTH).expect("MAX_PBUFFER_WIDTH present");
            let max_h = get(g::GLX_MAX_PBUFFER_HEIGHT).expect("MAX_PBUFFER_HEIGHT present");
            assert!(max_w > 0 && max_h > 0, "pbuffer caps must be non-zero");
            assert!(get(g::GLX_MAX_PBUFFER_PIXELS).is_some());
        }
    }
}

#[test]
fn glx_visuals_have_one_unambiguous_double_buffered_config() {
    use yserver_protocol::x11::glx as g;

    let visuals = synthesise_glx_visual_configs();
    assert_eq!(visuals.len(), 3);
    assert!(visuals.iter().all(|visual| visual.double_buffer));

    let configs = synthesise_glx_fb_configs(false);
    assert_eq!(configs.len(), 4);
    let mut visual_ids = HashSet::new();
    for config in configs {
        let get = |attr: u32| config.iter().find(|(a, _)| *a == attr).map(|(_, v)| *v);
        let visual_id = get(g::GLX_VISUAL_ID).expect("VISUAL_ID present");
        if visual_id == 0 {
            assert_eq!(get(g::GLX_DRAWABLE_TYPE), Some(g::GLX_PIXMAP_BIT));
            assert_eq!(get(g::GLX_DOUBLEBUFFER), Some(0));
        } else {
            assert!(
                visual_ids.insert(visual_id),
                "duplicate visual 0x{visual_id:x}"
            );
            assert_eq!(get(g::GLX_DOUBLEBUFFER), Some(1));
        }
    }
    assert_eq!(visual_ids.len(), 3);
}

// glmark2 2023.01 defaults to stencil=0 and rejects any positive
// stencil count while choosing its default FBConfig. Keep that config on
// a distinct X visual: reusing ROOT_VISUAL would recreate the config /
// visual ambiguity that made Mesa allocate fake front buffers (#96).
#[test]
fn glmark_can_choose_an_opaque_zero_stencil_window_config() {
    use yserver_protocol::x11::glx as g;

    let config = synthesise_glx_fb_configs(false)
        .into_iter()
        .find(|config| {
            let get = |attribute| {
                config
                    .iter()
                    .find(|(key, _)| *key == attribute)
                    .map(|(_, value)| *value)
            };
            get(g::GLX_DRAWABLE_TYPE).is_some_and(|value| value & g::GLX_WINDOW_BIT != 0)
                && get(g::GLX_DOUBLEBUFFER) == Some(1)
                && get(g::GLX_STENCIL_SIZE) == Some(0)
        })
        .expect("a double-buffered stencil-0 window FBConfig for glmark2");
    let get = |attribute| {
        config
            .iter()
            .find(|(key, _)| *key == attribute)
            .map(|(_, value)| *value)
    };
    assert_ne!(get(g::GLX_VISUAL_ID), Some(crate::resources::ROOT_VISUAL.0));
    assert_ne!(get(g::GLX_VISUAL_ID), Some(crate::resources::ARGB_VISUAL.0));
    assert_eq!(
        get(g::GLX_VISUAL_ID),
        Some(crate::resources::GLMARK_VISUAL.0)
    );
}

// QtWebEngine's GLXHelper chooses a native-pixmap config with this exact
// attribute set before it imports a DMA-BUF through DRI3.  In particular,
// it requires GLX_DOUBLEBUFFER=false.  It must not reuse either real X
// visual: doing so recreates #96, where Mesa paired a single-buffered
// context with a double-buffered window and allocated a fake front buffer.
#[test]
fn qtwebengine_can_choose_an_isolated_single_buffered_pixmap_config() {
    use yserver_protocol::x11::glx as g;

    let configs = synthesise_glx_fb_configs(true);
    let config = configs
        .iter()
        .find(|config| {
            let get = |attr: u32| {
                config
                    .iter()
                    .find(|(key, _)| *key == attr)
                    .map(|(_, value)| *value)
            };
            get(g::GLX_RED_SIZE).is_some_and(|value| value >= 8)
                && get(g::GLX_GREEN_SIZE).is_some_and(|value| value >= 8)
                && get(g::GLX_BLUE_SIZE).is_some_and(|value| value >= 8)
                && get(g::GLX_ALPHA_SIZE).is_some_and(|value| value >= 8)
                && get(g::GLX_BUFFER_SIZE).is_some_and(|value| value >= 32)
                && get(g::GLX_BIND_TO_TEXTURE_RGBA_EXT) == Some(1)
                && get(g::GLX_DRAWABLE_TYPE).is_some_and(|value| value & g::GLX_PIXMAP_BIT != 0)
                && get(g::GLX_BIND_TO_TEXTURE_TARGETS_EXT)
                    .is_some_and(|value| value & g::GLX_TEXTURE_2D_BIT_EXT != 0)
                && get(g::GLX_DOUBLEBUFFER) == Some(0)
        })
        .expect("a QtWebEngine native-pixmap FBConfig");
    let get = |attr: u32| {
        config
            .iter()
            .find(|(key, _)| *key == attr)
            .map(|(_, value)| *value)
    };
    assert_eq!(get(g::GLX_VISUAL_ID), Some(0));
    assert_eq!(get(g::GLX_DRAWABLE_TYPE), Some(g::GLX_PIXMAP_BIT));
    assert_eq!(get(g::GLX_X_RENDERABLE), Some(0));
}

// #96: pbuffer GetGeometry must report the fbconfig's true depth so Mesa's
// loader_dri3 backs it correctly. fbconfig 0x101 maps to ROOT_VISUAL
// (depth 24), while 0x103 maps to ARGB_VISUAL (depth 32).
#[test]
fn glx_fbconfig_depth_tracks_visual() {
    assert_eq!(glx_fbconfig_depth(0x101), 24);
    assert_eq!(glx_fbconfig_depth(0x103), 32);
    // Unknown fbconfig falls back to the depth-24 default.
    assert_eq!(glx_fbconfig_depth(0xDEAD), 24);
}

/// Every GLX visual `GetVisualConfigs` advertises has an FBConfig carrying
/// that visual (Xorg's `pGlxScreen->visuals[i]` *is* a config), and the
/// pairing round-trips. FBConfig IDs and the visual-less FBConfig are
/// not GLX visuals.
#[test]
fn glx_visual_fbconfig_pairs_every_advertised_visual() {
    for visual in synthesise_glx_visual_configs() {
        let fbconfig = glx_visual_fbconfig(visual.visual_id)
            .unwrap_or_else(|| panic!("visual 0x{:x} has no FBConfig", visual.visual_id));
        assert_eq!(glx_fbconfig_visual(fbconfig), visual.visual_id);
    }
    assert_eq!(glx_visual_fbconfig(0x101), None);
    assert_eq!(glx_visual_fbconfig(0x104), None);
    assert_eq!(glx_fbconfig_visual(0x104), 0);
    assert_eq!(glx_fbconfig_visual(0xDEAD), 0);
}

#[test]
fn glx_extension_string_includes_tfp_only_when_capable() {
    let with = glx_extension_string(true);
    let without = glx_extension_string(false);
    assert!(with.contains("GLX_EXT_texture_from_pixmap"));
    assert!(!without.contains("GLX_EXT_texture_from_pixmap"));
    // Base extensions always present.
    assert!(with.contains("GLX_ARB_create_context"));
    assert!(without.contains("GLX_ARB_create_context"));
}

/// Xorg writes a space after **every** enabled extension
/// (`glx/extension_string.c:144-145`), so its GLX extension string
/// always ends with `' '` before the NUL. yserver's did not, and
/// `libGLX_nvidia` responds by losing the final token *and* dropping
/// `GLX_ARB_get_proc_address` from the client extension string — which
/// makes libepoxy abort and takes `kwin_x11` down with SIGABRT.
///
/// Measured 2026-08-07; see
/// `docs/superpowers/specs/2026-08-07-glx-extension-string-terminator-design.md`
/// and `~/yserver-glx-logs/2026-08-07-terminator-evidence/`.
#[test]
fn glx_extension_string_is_space_terminated() {
    for tfp_supported in [false, true] {
        let s = glx_extension_string(tfp_supported);
        assert!(
            s.ends_with(' '),
            "GLX extension string must end with Xorg's space terminator; \
                 tfp_supported={tfp_supported} produced {s:?}"
        );
    }
}

/// Companion to `glx_extension_string_is_space_terminated`: the
/// terminator must not come at the cost of the token list. Green both
/// before and after that fix — this is a guard, not a regression test.
#[test]
fn glx_extension_string_tokens_match_advertised_constants() {
    use yserver_protocol::x11::glx as g;
    for tfp_supported in [false, true] {
        let s = glx_extension_string(tfp_supported);
        assert!(
            !s.starts_with(' '),
            "no leading separator; tfp_supported={tfp_supported} produced {s:?}"
        );
        assert!(
            !s.contains("  "),
            "no doubled separator; tfp_supported={tfp_supported} produced {s:?}"
        );
        let mut expected: Vec<&str> = g::SERVER_EXTENSIONS.split_whitespace().collect();
        expected.push(g::SGIX_FBCONFIG_EXTENSION);
        if tfp_supported {
            expected.push(g::TFP_EXTENSION);
        }
        let tokens: Vec<&str> = s.split_whitespace().collect();
        assert_eq!(
            tokens, expected,
            "token list must be the advertised constants, in order; \
                 tfp_supported={tfp_supported}"
        );
    }
}

#[test]
fn fbconfigs_emit_bind_to_texture_pairs_in_xorg_order() {
    use yserver_protocol::x11::glx as g;
    let configs = synthesise_glx_fb_configs(true);
    // All configs same length (wire encoder requirement).
    let len = configs[0].len();
    assert!(configs.iter().all(|c| c.len() == len));

    for c in &configs {
        // Find the bind-to-texture block; it must appear as a contiguous run in this order.
        let idx = c
            .iter()
            .position(|(k, _)| *k == g::GLX_BIND_TO_TEXTURE_RGB_EXT)
            .expect("RGB present");
        let order = [
            g::GLX_BIND_TO_TEXTURE_RGB_EXT,
            g::GLX_BIND_TO_TEXTURE_RGBA_EXT,
            g::GLX_BIND_TO_MIPMAP_TEXTURE_EXT,
            g::GLX_BIND_TO_TEXTURE_TARGETS_EXT,
            g::GLX_Y_INVERTED_EXT,
        ];
        for (i, key) in order.iter().enumerate() {
            assert_eq!(c[idx + i].0, *key, "wrong order at offset {i}");
        }
        // Values:
        assert_eq!(c[idx + 2].1, 0, "MIPMAP must be 0/false");
        assert_eq!(
            c[idx + 3].1,
            g::GLX_DONT_CARE,
            "BIND_TO_TEXTURE_TARGETS is GLX_DONT_CARE so it matches any \
                 client-driver target bitmask (driConfigEqual)"
        );
        assert_eq!(
            c[idx + 4].1,
            g::GLX_DONT_CARE,
            "Y_INVERTED in FBConfig is GLX_DONT_CARE"
        );
    }

    // Both depths advertise RGB=true RGBA=true, matching radeonsi's
    // TFP configs (HW-verified). RGBA=true on depth-24 is correct:
    // opaque windows sample as α=1.
    let depth24 = configs
        .iter()
        .find(|c| c.iter().any(|(k, v)| *k == g::GLX_VISUAL_ID && *v == 0x102))
        .unwrap();
    let d24 = depth24
        .iter()
        .position(|(k, _)| *k == g::GLX_BIND_TO_TEXTURE_RGB_EXT)
        .unwrap();
    assert_eq!(depth24[d24].1, 1); // RGB true
    assert_eq!(depth24[d24 + 1].1, 1); // RGBA true on depth-24 (match radeonsi)

    let depth32 = configs
        .iter()
        .find(|c| c.iter().any(|(k, v)| *k == g::GLX_VISUAL_ID && *v == 0x103))
        .unwrap();
    let d32 = depth32
        .iter()
        .position(|(k, _)| *k == g::GLX_BIND_TO_TEXTURE_RGB_EXT)
        .unwrap();
    assert_eq!(depth32[d32 + 1].1, 1); // RGBA true on depth-32
}

#[test]
fn fbconfigs_omit_bind_to_texture_when_tfp_unsupported() {
    use yserver_protocol::x11::glx as g;
    let configs = synthesise_glx_fb_configs(false);
    assert!(
        configs
            .iter()
            .all(|c| c.iter().all(|(k, _)| *k != g::GLX_BIND_TO_TEXTURE_RGB_EXT))
    );
    // Still equal length across configs.
    let len = configs[0].len();
    assert!(configs.iter().all(|c| c.len() == len));
}

/// GLX Task 3.4: `glXCreatePixmap` records the X drawable in
/// `glx_drawables`, calls `acquire_glx_pixmap_export` on the backend,
/// and `glXDestroyPixmap` clears the entry and calls
/// `release_glx_pixmap_export`.
#[test]
fn glx_create_pixmap_records_x_drawable_and_destroy_clears_it() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Register an X pixmap in the resource table with a known host_xid.
    let x_pixmap_xid: u32 = 0x2000;
    let host_xid_raw: u32 = 0xdead_0001;
    state.resources.create_pixmap(
        client_id,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(x_pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 32,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            ResourceId(x_pixmap_xid),
            crate::backend::PixmapHandle::from_raw(host_xid_raw).unwrap(),
        ),
        "set_pixmap_host_xid must succeed"
    );

    let glx_xid: u32 = 0x4000_0001;
    let fbconfig: u32 = 0x101;

    // Build a GLX CREATE_PIXMAP request body:
    // [screen(u32)][fbconfig(u32)][x_window(u32)][glx_window(u32)]
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&0u32.to_le_bytes()); // screen = 0
    create_body.extend_from_slice(&fbconfig.to_le_bytes());
    create_body.extend_from_slice(&x_pixmap_xid.to_le_bytes());
    create_body.extend_from_slice(&glx_xid.to_le_bytes());

    let length_units = u32::try_from(1 + create_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,                 // GLX_MAJOR_OPCODE
            data: x11glx::CREATE_PIXMAP, // minor = 22
            length_units,
        },
        &create_body,
        None,
    )
    .expect("process_request CREATE_PIXMAP");

    // Verify the GlxDrawable was recorded.
    let d = state
        .glx_drawables
        .get(&glx_xid)
        .expect("GlxDrawable must be present after CreatePixmap");
    assert_eq!(
        d.x_drawable, x_pixmap_xid,
        "x_drawable must point to the X pixmap"
    );
    assert_eq!(d.fbconfig, fbconfig);

    // Verify acquire was forwarded to the backend.
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::AcquireGlxPixmapExport(host_xid_raw)),
        "AcquireGlxPixmapExport must be recorded after CreatePixmap"
    );

    // Build a GLX DESTROY_PIXMAP request body: [glx_xid(u32)]
    let destroy_body = glx_xid.to_le_bytes().to_vec();
    let length_units = u32::try_from(1 + destroy_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(2),
        RequestHeader {
            opcode: 148,
            data: x11glx::DESTROY_PIXMAP, // minor = 23
            length_units,
        },
        &destroy_body,
        None,
    )
    .expect("process_request DESTROY_PIXMAP");

    // GlxDrawable must be removed.
    assert!(
        !state.glx_drawables.contains_key(&glx_xid),
        "GlxDrawable must be removed after DestroyPixmap"
    );

    // Verify release was forwarded to the backend.
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::ReleaseGlxPixmapExport(host_xid_raw)),
        "ReleaseGlxPixmapExport must be recorded after DestroyPixmap"
    );
}

// ─── GLX 1.0 CreateGLXPixmap / DestroyGLXPixmap, QueryContext, IsDirect ───
//
// Expected wire behaviour is Xorg's, captured from Xvfb 21.1.24
// (llvmpipe GLX) with an xcb probe on 2026-09-26. Xvfb's GLX visual 0x21
// carries FBConfig 0x88; the yserver counterpart used below is
// ROOT_VISUAL 0x102 carrying FBConfig 0x101 (synthesise_glx_fb_configs).
// Error values, minors and the order in which the checks fire are
// Xvfb's, verbatim.

const GLX_TEST_XID_BASE: u32 = 0x0020_0000;
const GLX_TEST_XID_MASK: u32 = 0x001f_ffff;

/// A client whose XID range is `0x0020_0000 | 0x001f_ffff`, the range
/// Xvfb handed the capture probe, so `LEGAL_NEW_RESOURCE` is exercised.
fn glx_legacy_fixture() -> (ServerState, RecordingBackend, UnixStream) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, 1);
    let client = state.clients.get_mut(&1).expect("test client");
    client.resource_id_base = GLX_TEST_XID_BASE;
    client.resource_id_mask = GLX_TEST_XID_MASK;
    (state, RecordingBackend::new(), peer)
}

fn glx_legacy_x_pixmap(state: &mut ServerState, xid: u32, depth: u8, host: Option<u32>) {
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth,
            pixmap: ResourceId(xid),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 32,
        },
    );
    if let Some(host) = host {
        assert!(state.resources.set_pixmap_host_xid(
            ResourceId(xid),
            crate::backend::PixmapHandle::from_raw(host).expect("non-zero host xid"),
        ));
    }
}

fn glx_legacy_words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Run one request for `client` and return every byte it wrote.
fn glx_legacy_send_major(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    client: u32,
    (opcode, minor): (u8, u8),
    body: &[u8],
) -> Vec<u8> {
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");
    process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(7),
        RequestHeader {
            opcode,
            data: minor,
            length_units,
        },
        body,
        None,
    )
    .expect("process_request");
    read_all_or_buffered(state, client, peer)
}

/// Run one GLX request for `client` and return every byte it wrote.
fn glx_legacy_send(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    client: u32,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    glx_legacy_send_major(
        state,
        backend,
        peer,
        client,
        (crate::nested::GLX_MAJOR_OPCODE, minor),
        body,
    )
}

/// Assert `bytes` is exactly one X error with this code, bad value and
/// minor opcode on the GLX major opcode.
fn assert_glx_legacy_error(what: &str, bytes: &[u8], code: u8, value: u32, minor: u8) {
    assert_eq!(bytes.len(), 32, "{what}: expected exactly one error packet");
    assert_eq!(bytes[0], 0, "{what}: expected an X error");
    assert_eq!(bytes[1], code, "{what}: error code");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        value,
        "{what}: bad value"
    );
    assert_eq!(
        u16::from_le_bytes([bytes[8], bytes[9]]),
        u16::from(minor),
        "{what}: minor opcode"
    );
    assert_eq!(
        bytes[10],
        crate::nested::GLX_MAJOR_OPCODE,
        "{what}: major opcode"
    );
}

fn create_glx_pixmap_body(screen: u32, visual: u32, pixmap: u32, glx_pixmap: u32) -> Vec<u8> {
    glx_legacy_words(&[screen, visual, pixmap, glx_pixmap])
}

/// Decode the (attribute, value) pairs of a GetDrawableAttributes /
/// QueryContext reply, checking the header's length and count agree.
fn glx_legacy_reply_pairs(bytes: &[u8]) -> Vec<(u32, u32)> {
    assert_eq!(bytes[0], 1, "expected a reply, got {bytes:02x?}");
    let length = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    assert_eq!(length, 2 * n, "reply length must be 2 * n");
    assert_eq!(bytes.len(), 32 + 8 * n, "reply size");
    (0..n)
        .map(|i| {
            let at = 32 + 8 * i;
            (
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
                u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
            )
        })
        .collect()
}

/// GLX 1.0 `glXCreateGLXPixmap` (minor 13) used to hit the
/// `GLXBadRenderRequest` catch-all, so Xlib's default handler killed the
/// app. It must create the same GLXPixmap record GLX 1.3 `CreatePixmap`
/// does (config = the visual's FBConfig), hold the export ref, report
/// Xorg's drawable attributes, and `DestroyGLXPixmap` (15) must release
/// the ref — also after the X pixmap was freed first (Xorg bumps the
/// pixmap refcount, glxcmds.c `DoCreateGLXPixmap`).
#[test]
fn glx10_create_glx_pixmap_records_pixmap_and_destroy_releases_it() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::glx as g;

    let (mut state, mut backend, mut peer) = glx_legacy_fixture();
    let pixmap = GLX_TEST_XID_BASE | 0x01;
    let glx_pixmap = GLX_TEST_XID_BASE | 0x02;
    let host = 0xdead_0001;
    glx_legacy_x_pixmap(&mut state, pixmap, 24, Some(host));

    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::CREATE_GLX_PIXMAP,
        &create_glx_pixmap_body(0, crate::resources::ROOT_VISUAL.0, pixmap, glx_pixmap),
    );
    assert!(
        out.is_empty(),
        "CreateGLXPixmap succeeds silently, got {out:02x?}"
    );
    let record = state
        .glx_drawables
        .get(&glx_pixmap)
        .expect("GLXPixmap recorded");
    assert_eq!(record.kind, crate::server::GlxDrawableKind::Pixmap);
    assert_eq!(record.x_drawable, pixmap);
    assert_eq!(record.fbconfig, 0x101, "the visual's FBConfig");
    assert_eq!(record.glx_export_host_xid, Some(host));
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::AcquireGlxPixmapExport(host))
    );

    // Xvfb: 0x20d4=0 0x801d=64 0x801e=32 0x800c=0 0x20d6=0x20dd
    //       0x801f=0 0x8013=<fbconfig> 0x8010=GLX_PIXMAP_BIT
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::GET_DRAWABLE_ATTRIBUTES,
        &glx_pixmap.to_le_bytes(),
    );
    assert_eq!(
        glx_legacy_reply_pairs(&out),
        vec![
            (g::GLX_Y_INVERTED_EXT, 0),
            (g::GLX_WIDTH, 64),
            (g::GLX_HEIGHT, 32),
            (g::GLX_SCREEN, 0),
            (g::GLX_TEXTURE_TARGET_EXT, g::GLX_TEXTURE_RECTANGLE_EXT),
            (g::GLX_EVENT_MASK, 0),
            (g::GLX_FBCONFIG_ID, 0x101),
            (g::GLX_DRAWABLE_TYPE, g::GLX_PIXMAP_BIT),
        ]
    );

    // FreePixmap first, then DestroyGLXPixmap: both Success on Xvfb.
    let out = glx_legacy_send_major(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        (54, 0),
        &pixmap.to_le_bytes(),
    );
    assert!(out.is_empty(), "FreePixmap: got {out:02x?}");
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::DESTROY_GLX_PIXMAP,
        &glx_pixmap.to_le_bytes(),
    );
    assert!(
        out.is_empty(),
        "DestroyGLXPixmap succeeds silently, got {out:02x?}"
    );
    assert!(!state.glx_drawables.contains_key(&glx_pixmap));
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::ReleaseGlxPixmapExport(host))
    );
}

/// Every CreateGLXPixmap failure Xvfb produced, with its error code,
/// bad value and the precedence between checks: size, then the new
/// XID (`LEGAL_NEW_RESOURCE`), then the screen (GLXVND: BadMatch), then
/// the visual (BadValue), then the pixmap (core BadDrawable, or core
/// BadPixmap for a window). A failed request creates nothing.
#[test]
fn glx10_create_glx_pixmap_errors_match_xorg() {
    use yserver_protocol::x11::{error, glx as g};

    let (mut state, mut backend, mut peer) = glx_legacy_fixture();
    let pixmap = GLX_TEST_XID_BASE | 0x01;
    let other_pixmap = GLX_TEST_XID_BASE | 0x03;
    let existing_glx = GLX_TEST_XID_BASE | 0x04;
    let fresh = GLX_TEST_XID_BASE | 0x05;
    let vis = crate::resources::ROOT_VISUAL.0;
    glx_legacy_x_pixmap(&mut state, pixmap, 24, None);
    glx_legacy_x_pixmap(&mut state, other_pixmap, 32, None);
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::CREATE_GLX_PIXMAP,
        &create_glx_pixmap_body(0, vis, pixmap, existing_glx),
    );
    assert!(out.is_empty());

    let bad_id = error::BAD_ID_CHOICE;
    let unknown = 0x5a_5a5a;
    let root = ROOT_WINDOW.0;
    let cases: &[(&str, [u32; 4], u8, u32)] = &[
        (
            "glx id in use",
            [0, vis, pixmap, existing_glx],
            bad_id,
            existing_glx,
        ),
        (
            "glx id = X pixmap",
            [0, vis, pixmap, other_pixmap],
            bad_id,
            other_pixmap,
        ),
        (
            "glx id out of range",
            [0, vis, pixmap, 0x07f0_0001],
            bad_id,
            0x07f0_0001,
        ),
        ("screen 1", [1, vis, pixmap, fresh], error::BAD_MATCH, 1),
        (
            "unknown visual",
            [0, 0xdead, pixmap, fresh],
            error::BAD_VALUE,
            0xdead,
        ),
        // An FBConfig ID is not a GLX visual (Xvfb: its visual-less
        // FBConfig 0x41 → BadValue 0x41).
        (
            "visual-less fbconfig id",
            [0, 0x104, pixmap, fresh],
            error::BAD_VALUE,
            0x104,
        ),
        (
            "fbconfig id of a visual",
            [0, 0x101, pixmap, fresh],
            error::BAD_VALUE,
            0x101,
        ),
        (
            "screen 1 + bad visual",
            [1, 0xdead, pixmap, fresh],
            error::BAD_MATCH,
            1,
        ),
        (
            "root window as pixmap",
            [0, vis, root, fresh],
            error::BAD_PIXMAP,
            root,
        ),
        (
            "unknown pixmap",
            [0, vis, unknown, fresh],
            error::BAD_DRAWABLE,
            unknown,
        ),
        (
            "bad visual + bad pixmap",
            [0, 0xdead, unknown, fresh],
            error::BAD_VALUE,
            0xdead,
        ),
        (
            "bad pixmap + glx id in use",
            [0, vis, unknown, existing_glx],
            bad_id,
            existing_glx,
        ),
    ];
    for (what, [screen, visual, pix, glx], code, value) in cases {
        let out = glx_legacy_send(
            &mut state,
            &mut backend,
            &mut peer,
            1,
            g::CREATE_GLX_PIXMAP,
            &create_glx_pixmap_body(*screen, *visual, *pix, *glx),
        );
        assert_glx_legacy_error(what, &out, *code, *value, g::CREATE_GLX_PIXMAP);
    }
    assert!(!state.glx_drawables.contains_key(&fresh));
    assert!(!state.glx_drawables.contains_key(&other_pixmap));
    assert_eq!(state.glx_drawables.len(), 1);
}

/// Xorg does not compare the pixmap depth with the visual: Xvfb accepted
/// depth-32, depth-8 and depth-1 pixmaps for its depth-24 visual.
#[test]
fn glx10_create_glx_pixmap_accepts_any_pixmap_depth() {
    use yserver_protocol::x11::glx as g;

    let (mut state, mut backend, mut peer) = glx_legacy_fixture();
    for (i, depth) in (0_u32..).zip([32_u8, 8, 1]) {
        let pixmap = GLX_TEST_XID_BASE | (0x10 + i);
        let glx_pixmap = GLX_TEST_XID_BASE | (0x20 + i);
        glx_legacy_x_pixmap(&mut state, pixmap, depth, None);
        let out = glx_legacy_send(
            &mut state,
            &mut backend,
            &mut peer,
            1,
            g::CREATE_GLX_PIXMAP,
            &create_glx_pixmap_body(0, crate::resources::ROOT_VISUAL.0, pixmap, glx_pixmap),
        );
        assert!(out.is_empty(), "depth {depth}: got {out:02x?}");
        assert!(state.glx_drawables.contains_key(&glx_pixmap));
    }
}

/// DestroyGLXPixmap takes only a live GLX *pixmap*: anything else is
/// `GLXBadPixmap` with the XID as bad value (GLXVND's XID map, then
/// `validGlxDrawable`'s type check). GLX 1.0 and GLX 1.3 pixmaps are the
/// same resource type, so either destroy request frees either.
#[test]
fn glx10_destroy_glx_pixmap_errors_and_cross_version_destroy_match_xorg() {
    use yserver_protocol::x11::glx as g;

    let (mut state, mut backend, mut peer) = glx_legacy_fixture();
    let bad_pixmap = crate::nested::GLX_FIRST_ERROR + g::ERROR_GLX_BAD_PIXMAP;
    let pixmap = GLX_TEST_XID_BASE | 0x01;
    glx_legacy_x_pixmap(&mut state, pixmap, 24, None);
    let legacy = GLX_TEST_XID_BASE | 0x02;
    let modern = GLX_TEST_XID_BASE | 0x03;
    let glx_window = GLX_TEST_XID_BASE | 0x04;
    let pbuffer = GLX_TEST_XID_BASE | 0x05;
    let vis = crate::resources::ROOT_VISUAL.0;

    let mut send = |state: &mut ServerState, minor: u8, body: &[u8]| {
        glx_legacy_send(state, &mut backend, &mut peer, 1, minor, body)
    };
    let window_body = glx_legacy_words(&[0, 0x101, ROOT_WINDOW.0, glx_window]);
    assert!(send(&mut state, g::CREATE_WINDOW, &window_body).is_empty());
    let pbuffer_body = glx_legacy_words(&[0, 0x101, pbuffer, 0]);
    assert!(send(&mut state, g::CREATE_PBUFFER, &pbuffer_body).is_empty());

    for (what, xid) in [
        ("unknown xid", 0x5a_5a5a),
        ("X pixmap", pixmap),
        ("GLX window", glx_window),
        ("GLX pbuffer", pbuffer),
    ] {
        let out = send(&mut state, g::DESTROY_GLX_PIXMAP, &xid.to_le_bytes());
        assert_glx_legacy_error(what, &out, bad_pixmap, xid, g::DESTROY_GLX_PIXMAP);
    }
    assert!(state.glx_drawables.contains_key(&glx_window));
    assert!(state.glx_drawables.contains_key(&pbuffer));

    // GLX 1.0 pixmap destroyed by GLX 1.3 DestroyPixmap; a second
    // destroy through DestroyGLXPixmap is GLXBadPixmap.
    let body = create_glx_pixmap_body(0, vis, pixmap, legacy);
    assert!(send(&mut state, g::CREATE_GLX_PIXMAP, &body).is_empty());
    assert!(send(&mut state, g::DESTROY_PIXMAP, &legacy.to_le_bytes()).is_empty());
    assert!(!state.glx_drawables.contains_key(&legacy));
    let out = send(&mut state, g::DESTROY_GLX_PIXMAP, &legacy.to_le_bytes());
    assert_glx_legacy_error(
        "destroyed twice",
        &out,
        bad_pixmap,
        legacy,
        g::DESTROY_GLX_PIXMAP,
    );

    // GLX 1.3 pixmap destroyed by GLX 1.0 DestroyGLXPixmap.
    let body = glx_legacy_words(&[0, 0x101, pixmap, modern, 0]);
    assert!(send(&mut state, g::CREATE_PIXMAP, &body).is_empty());
    assert!(send(&mut state, g::DESTROY_GLX_PIXMAP, &modern.to_le_bytes()).is_empty());
    assert!(!state.glx_drawables.contains_key(&modern));
}

/// CreateGLXPixmap, DestroyGLXPixmap, QueryContext and IsDirect are all
/// `REQUEST_SIZE_MATCH` in Xorg's GLXVND stubs: a short or long request
/// is core `BadLength`, and nothing is created.
#[test]
fn glx10_fixed_size_requests_reject_wrong_length() {
    use yserver_protocol::x11::{error, glx as g};

    let (mut state, mut backend, mut peer) = glx_legacy_fixture();
    let pixmap = GLX_TEST_XID_BASE | 0x01;
    glx_legacy_x_pixmap(&mut state, pixmap, 24, None);
    let mut long_create = create_glx_pixmap_body(
        0,
        crate::resources::ROOT_VISUAL.0,
        pixmap,
        GLX_TEST_XID_BASE | 0x02,
    );
    long_create.extend_from_slice(&[0; 4]);
    let short_create = &long_create[..12];
    for (what, minor, body) in [
        (
            "CreateGLXPixmap long",
            g::CREATE_GLX_PIXMAP,
            long_create.as_slice(),
        ),
        ("CreateGLXPixmap short", g::CREATE_GLX_PIXMAP, short_create),
        ("DestroyGLXPixmap long", g::DESTROY_GLX_PIXMAP, &[0; 8][..]),
        ("DestroyGLXPixmap empty", g::DESTROY_GLX_PIXMAP, &[][..]),
        ("QueryContext long", g::QUERY_CONTEXT, &[0; 8][..]),
        ("QueryContext empty", g::QUERY_CONTEXT, &[][..]),
        ("IsDirect long", g::IS_DIRECT, &[0; 8][..]),
        ("IsDirect empty", g::IS_DIRECT, &[][..]),
    ] {
        let out = glx_legacy_send(&mut state, &mut backend, &mut peer, 1, minor, body);
        assert_glx_legacy_error(what, &out, error::BAD_LENGTH, 0, minor);
    }
    assert!(state.glx_drawables.is_empty());
}

/// `QueryContext` (GLX_EXT_import_context) answers Xorg's five
/// attributes in Xorg's order (glxcmds.c `DoQueryContext`) for every
/// creation request, to any client, and `GLXBadContext` for an XID that
/// is not a live context. `IsDirect` reports the creation request's
/// `isDirect` and shares the `GLXBadContext` rule.
#[test]
fn glx_query_context_and_is_direct_match_xorg() {
    use yserver_protocol::x11::glx as g;

    let (mut state, mut backend, mut peer) = glx_legacy_fixture();
    let mut peer2 = install_client(&mut state, 2);
    let bad_context = crate::nested::GLX_FIRST_ERROR + g::ERROR_GLX_BAD_CONTEXT;
    let root_visual = crate::resources::ROOT_VISUAL.0;
    let ctx1 = GLX_TEST_XID_BASE | 0x11;
    let ctx2 = GLX_TEST_XID_BASE | 0x12;
    let ctx3 = GLX_TEST_XID_BASE | 0x13;
    let ctx4 = GLX_TEST_XID_BASE | 0x14;
    let ctx5 = GLX_TEST_XID_BASE | 0x15;

    let creates: [(u8, Vec<u32>); 5] = [
        // CreateContext: context visual screen share isDirect
        (g::CREATE_CONTEXT, vec![ctx1, root_visual, 0, 0, 1]),
        // CreateNewContext (visual-less FBConfig, share ctx1):
        // context fbconfig screen renderType share isDirect
        (
            g::CREATE_NEW_CONTEXT,
            vec![ctx2, 0x104, 0, g::GLX_RGBA_TYPE, ctx1, 1],
        ),
        // CreateContextAttribsARB (share ctx1, GL 3.0):
        // context fbconfig screen share isDirect numAttribs attribs
        (
            g::CREATE_CONTEXT_ATTRIBS_ARB,
            vec![ctx3, 0x101, 0, ctx1, 1, 2, 0x2091, 3, 0x2092, 0],
        ),
        // CreateContextAttribsARB with GLX_RENDER_TYPE = GLX_RGBA_TYPE.
        (
            g::CREATE_CONTEXT_ATTRIBS_ARB,
            vec![
                ctx4,
                0x101,
                0,
                0,
                1,
                1,
                g::GLX_RENDER_TYPE,
                g::GLX_RGBA_TYPE,
            ],
        ),
        // CreateContext asking for an indirect context (isDirect 0).
        (g::CREATE_CONTEXT, vec![ctx5, root_visual, 0, 0, 0]),
    ];
    for (minor, words) in &creates {
        let body = glx_legacy_words(words);
        let out = glx_legacy_send(&mut state, &mut backend, &mut peer, 1, *minor, &body);
        assert!(out.is_empty(), "create minor {minor}: got {out:02x?}");
    }

    let expect = |share: u32, visual: u32, fbconfig: u32| {
        vec![
            (g::GLX_SHARE_CONTEXT_EXT, share),
            (g::GLX_VISUAL_ID, visual),
            (g::GLX_SCREEN, 0),
            (g::GLX_FBCONFIG_ID, fbconfig),
            (g::GLX_RENDER_TYPE, g::GLX_RGBA_TYPE),
        ]
    };
    for (ctx, share, visual, fbconfig) in [
        // Xvfb: 0x800a=0 0x800b=0x21 0x800c=0 0x8013=0x88 0x8011=0x8014
        (ctx1, 0, root_visual, 0x101),
        // Xvfb: 0x800a=ctx1 0x800b=0 0x800c=0 0x8013=0x41 0x8011=0x8014
        (ctx2, ctx1, 0, 0x104),
        // Xvfb: 0x800a=ctx1 0x800b=0x21 0x800c=0 0x8013=0x88 0x8011=0x8014
        (ctx3, ctx1, root_visual, 0x101),
        // Xvfb: 0x800a=0 0x800b=0x21 0x800c=0 0x8013=0x88 0x8011=0x8014
        (ctx4, 0, root_visual, 0x101),
        // Xvfb +iglx: 0x800a=0 0x800b=0x21 0x800c=0 0x8013=0x88 0x8011=0x8014
        (ctx5, 0, root_visual, 0x101),
    ] {
        let out = glx_legacy_send(
            &mut state,
            &mut backend,
            &mut peer,
            1,
            g::QUERY_CONTEXT,
            &ctx.to_le_bytes(),
        );
        assert_eq!(
            glx_legacy_reply_pairs(&out),
            expect(share, visual, fbconfig),
            "QueryContext(0x{ctx:x})"
        );
    }

    // Another client may query and IsDirect it: that is what
    // import_context is for.
    let ctx1_body = ctx1.to_le_bytes();
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer2,
        2,
        g::QUERY_CONTEXT,
        &ctx1_body,
    );
    assert_eq!(glx_legacy_reply_pairs(&out), expect(0, root_visual, 0x101));
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer2,
        2,
        g::IS_DIRECT,
        &ctx1_body,
    );
    assert_eq!(out.len(), 32, "IsDirect reply is 32 bytes");
    assert_eq!(out[0], 1, "IsDirect reply");
    assert_eq!(&out[4..8], &[0; 4], "IsDirect reply length 0");
    assert_eq!(out[8], 1, "IsDirect(ctx1) = True");
    // Xvfb +iglx: IsDirect of a context created with isDirect 0 is 0.
    // (Without +iglx Xorg refuses that CreateContext with BadValue.)
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::IS_DIRECT,
        &ctx5.to_le_bytes(),
    );
    assert_eq!((out.len(), out[0], out[8]), (32, 1, 0), "IsDirect(ctx5)");

    let pixmap = GLX_TEST_XID_BASE | 0x01;
    let glx_pixmap = GLX_TEST_XID_BASE | 0x02;
    glx_legacy_x_pixmap(&mut state, pixmap, 24, None);
    let body = create_glx_pixmap_body(0, root_visual, pixmap, glx_pixmap);
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::CREATE_GLX_PIXMAP,
        &body,
    );
    assert!(out.is_empty());
    for (what, xid) in [
        ("unknown", 0x5a_5a5a),
        ("GLX pixmap", glx_pixmap),
        ("None", 0),
    ] {
        for minor in [g::QUERY_CONTEXT, g::IS_DIRECT] {
            let out = glx_legacy_send(
                &mut state,
                &mut backend,
                &mut peer,
                1,
                minor,
                &xid.to_le_bytes(),
            );
            assert_glx_legacy_error(what, &out, bad_context, xid, minor);
        }
    }

    let ctx2_body = ctx2.to_le_bytes();
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::DESTROY_CONTEXT,
        &ctx2_body,
    );
    assert!(out.is_empty());
    let out = glx_legacy_send(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        g::QUERY_CONTEXT,
        &ctx2_body,
    );
    assert_glx_legacy_error(
        "destroyed context",
        &out,
        bad_context,
        ctx2,
        g::QUERY_CONTEXT,
    );
}

/// `GetDrawableAttributes` on a GLXPixmap/GLXWindow must report the
/// geometry of the *backing* X drawable. The GLX XID is a fresh
/// client-allocated id with no X resource behind it, so resolving the
/// geometry from the GLX XID itself always missed and fell through to
/// `unwrap_or((0, 0))`. Measured on the wire against `libGLX_nvidia`
/// (2026-08-03): a 64×32 backing drawable was reported as
/// `GLX_WIDTH`/`GLX_HEIGHT` = 0. Xorg reads `pGlxDraw->pDraw->width` /
/// `->height` (glxcmds.c:1891). Against the old logic this test FAILS
/// with 0×0.
#[test]
fn glx_drawable_attributes_report_backing_drawable_geometry() {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    let x_pixmap_xid: u32 = 0x2000;
    state.resources.create_pixmap(
        client_id,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(x_pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 32,
        },
    );

    let glx_xid: u32 = 0x4000_0001;
    let fbconfig: u32 = 0x101;
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&0u32.to_le_bytes()); // screen = 0
    create_body.extend_from_slice(&fbconfig.to_le_bytes());
    create_body.extend_from_slice(&x_pixmap_xid.to_le_bytes());
    create_body.extend_from_slice(&glx_xid.to_le_bytes());
    let length_units = u32::try_from(1 + create_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: g::CREATE_PIXMAP,
            length_units,
        },
        &create_body,
        None,
    )
    .expect("process_request CREATE_PIXMAP");

    let attribs = drawable_attributes_for(&state, glx_xid);
    let get = |key: u32| {
        attribs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("attribute 0x{key:x} must be present"))
    };
    assert_eq!(get(g::GLX_WIDTH), 64, "GLX_WIDTH must track the X pixmap");
    assert_eq!(get(g::GLX_HEIGHT), 32, "GLX_HEIGHT must track the X pixmap");
    assert_eq!(
        get(g::GLX_FBCONFIG_ID),
        fbconfig,
        "a registered GLX drawable must report its real fbconfig"
    );
}

/// Naked X window (GLX 1.2 pattern, no GLX record): Xorg skips the
/// whole `pGlxDraw` block — `GLX_FBCONFIG_ID` / `GLX_TEXTURE_TARGET_EXT`
/// / `GLX_EVENT_MASK` must be **absent**, not present-and-zero — and
/// still ends with `GLX_DRAWABLE_TYPE = GLX_WINDOW_BIT`
/// (glxcmds.c:1875-1914). Asserts exact set and order.
#[test]
fn glx_drawable_attributes_naked_window_matches_xorg() {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let client_id = ClientId(1);
    let window_xid: u32 = 0x2000;
    state.resources.create_window(
        client_id,
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(window_xid),
            parent: ROOT_WINDOW,
            width: 64,
            height: 32,
            ..Default::default()
        },
    );

    let attribs = drawable_attributes_for(&state, window_xid);
    assert_eq!(
        attribs,
        vec![
            (g::GLX_Y_INVERTED_EXT, 0),
            (g::GLX_WIDTH, 64),
            (g::GLX_HEIGHT, 32),
            (g::GLX_SCREEN, 0),
            (g::GLX_DRAWABLE_TYPE, g::GLX_WINDOW_BIT),
        ],
        "naked-window attribute set and order must match glxcmds.c:1889-1914"
    );
}

/// Registered GLXWindow: the `pGlxDraw` block is present, in Xorg's
/// order (glxcmds.c:1894-1906), with `GLX_STEREO_TREE_EXT = 0` for
/// window-type drawables and `GLX_DRAWABLE_TYPE = GLX_WINDOW_BIT` last.
#[test]
fn glx_drawable_attributes_glx_window_matches_xorg() {
    use crate::server::{GlxDrawable, GlxDrawableKind};
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let client_id = ClientId(1);
    let x_window: u32 = 0x2000;
    state.resources.create_window(
        client_id,
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(x_window),
            parent: ROOT_WINDOW,
            width: 64,
            height: 32,
            ..Default::default()
        },
    );
    let glx_xid: u32 = 0x4000_0001;
    state.glx_drawables.insert(
        glx_xid,
        GlxDrawable {
            owner: client_id,
            kind: GlxDrawableKind::Window,
            x_drawable: x_window,
            fbconfig: 0x101,
            width: 0,
            height: 0,
            event_mask: 0,
            glx_export_host_xid: None,
            texture_target: yserver_protocol::x11::glx::GLX_TEXTURE_2D_EXT,
        },
    );

    let attribs = drawable_attributes_for(&state, glx_xid);
    assert_eq!(
        attribs,
        vec![
            (g::GLX_Y_INVERTED_EXT, 0),
            (g::GLX_WIDTH, 64),
            (g::GLX_HEIGHT, 32),
            (g::GLX_SCREEN, 0),
            (g::GLX_TEXTURE_TARGET_EXT, g::GLX_TEXTURE_2D_EXT),
            (g::GLX_EVENT_MASK, 0),
            (g::GLX_FBCONFIG_ID, 0x101),
            (g::GLX_STEREO_TREE_EXT, 0),
            (g::GLX_DRAWABLE_TYPE, g::GLX_WINDOW_BIT),
        ],
        "GLXWindow attribute set and order must match glxcmds.c:1889-1914"
    );
}

/// Registered GLXPixmap: like GLXWindow but without
/// `GLX_STEREO_TREE_EXT` (window-only) and with
/// `GLX_DRAWABLE_TYPE = GLX_PIXMAP_BIT`.
#[test]
fn glx_drawable_attributes_glx_pixmap_matches_xorg() {
    use crate::server::{GlxDrawable, GlxDrawableKind};
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let client_id = ClientId(1);
    let x_pixmap: u32 = 0x2000;
    state.resources.create_pixmap(
        client_id,
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(x_pixmap),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 32,
        },
    );
    let glx_xid: u32 = 0x4000_0001;
    state.glx_drawables.insert(
        glx_xid,
        GlxDrawable {
            owner: client_id,
            kind: GlxDrawableKind::Pixmap,
            x_drawable: x_pixmap,
            fbconfig: 0x101,
            width: 0,
            height: 0,
            event_mask: 0,
            glx_export_host_xid: None,
            texture_target: yserver_protocol::x11::glx::GLX_TEXTURE_2D_EXT,
        },
    );

    let attribs = drawable_attributes_for(&state, glx_xid);
    assert_eq!(
        attribs,
        vec![
            (g::GLX_Y_INVERTED_EXT, 0),
            (g::GLX_WIDTH, 64),
            (g::GLX_HEIGHT, 32),
            (g::GLX_SCREEN, 0),
            (g::GLX_TEXTURE_TARGET_EXT, g::GLX_TEXTURE_2D_EXT),
            (g::GLX_EVENT_MASK, 0),
            (g::GLX_FBCONFIG_ID, 0x101),
            (g::GLX_DRAWABLE_TYPE, g::GLX_PIXMAP_BIT),
        ],
        "GLXPixmap attribute set and order must match glxcmds.c:1889-1914"
    );
}

/// Registered pbuffer: geometry from the record,
/// `GLX_PRESERVED_CONTENTS = 1` (pbuffer-only) and
/// `GLX_DRAWABLE_TYPE = GLX_PBUFFER_BIT`.
#[test]
fn glx_drawable_attributes_pbuffer_matches_xorg() {
    use crate::server::{GlxDrawable, GlxDrawableKind};
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let client_id = ClientId(1);
    let glx_xid: u32 = 0x4000_0001;
    state.glx_drawables.insert(
        glx_xid,
        GlxDrawable {
            owner: client_id,
            kind: GlxDrawableKind::Pbuffer,
            x_drawable: glx_xid,
            fbconfig: 0x101,
            width: 64,
            height: 32,
            event_mask: 0,
            glx_export_host_xid: None,
            texture_target: yserver_protocol::x11::glx::GLX_TEXTURE_2D_EXT,
        },
    );

    let attribs = drawable_attributes_for(&state, glx_xid);
    assert_eq!(
        attribs,
        vec![
            (g::GLX_Y_INVERTED_EXT, 0),
            (g::GLX_WIDTH, 64),
            (g::GLX_HEIGHT, 32),
            (g::GLX_SCREEN, 0),
            (g::GLX_TEXTURE_TARGET_EXT, g::GLX_TEXTURE_2D_EXT),
            (g::GLX_EVENT_MASK, 0),
            (g::GLX_FBCONFIG_ID, 0x101),
            (g::GLX_PRESERVED_CONTENTS, 1),
            (g::GLX_DRAWABLE_TYPE, g::GLX_PBUFFER_BIT),
        ],
        "pbuffer attribute set and order must match glxcmds.c:1889-1914"
    );
}

/// Request-level wire test: `GET_DRAWABLE_ATTRIBUTES` on a naked X
/// window must deliver a well-formed reply — `length = 2n` 4-byte
/// units, `numAttribs = n`, pairs in Xorg order — with no
/// `GLX_FBCONFIG_ID` anywhere in the payload.
#[test]
fn glx_get_drawable_attributes_naked_window_wire_reply() {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    let window_xid: u32 = 0x2000;
    state.resources.create_window(
        client_id,
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(window_xid),
            parent: ROOT_WINDOW,
            width: 64,
            height: 32,
            ..Default::default()
        },
    );

    let body = window_xid.to_le_bytes().to_vec();
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(7),
        RequestHeader {
            opcode: 148,
            data: g::GET_DRAWABLE_ATTRIBUTES,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request GET_DRAWABLE_ATTRIBUTES");

    let expected: [(u32, u32); 5] = [
        (g::GLX_Y_INVERTED_EXT, 0),
        (g::GLX_WIDTH, 64),
        (g::GLX_HEIGHT, 32),
        (g::GLX_SCREEN, 0),
        (g::GLX_DRAWABLE_TYPE, g::GLX_WINDOW_BIT),
    ];
    let n = u32::try_from(expected.len()).expect("fits");

    peer.set_nonblocking(true).unwrap();
    let mut header = [0u8; 32];
    peer.read_exact(&mut header)
        .expect("reply header delivered");
    assert_eq!(header[0], 1, "byte 0 must be 1 (Reply)");
    assert_eq!(u16::from_le_bytes([header[2], header[3]]), 7, "sequence");
    assert_eq!(
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
        2 * n,
        "length must be 2 * numAttribs 4-byte units"
    );
    assert_eq!(
        u32::from_le_bytes([header[8], header[9], header[10], header[11]]),
        n,
        "numAttribs"
    );

    let mut pairs = vec![0u8; (2 * n) as usize * 4];
    peer.read_exact(&mut pairs)
        .expect("attribute pairs delivered");
    for (i, &(key, value)) in expected.iter().enumerate() {
        let got_key = u32::from_le_bytes([
            pairs[i * 8],
            pairs[i * 8 + 1],
            pairs[i * 8 + 2],
            pairs[i * 8 + 3],
        ]);
        let got_value = u32::from_le_bytes([
            pairs[i * 8 + 4],
            pairs[i * 8 + 5],
            pairs[i * 8 + 6],
            pairs[i * 8 + 7],
        ]);
        assert_eq!((got_key, got_value), (key, value), "pair {i}");
    }
}

#[test]
fn glx_vendor_names_query_answers_from_server_state() {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // The value the backend derived at startup, not the constant.
    state.glx_vendor_names = "nvidia mesa".to_string();

    // QueryServerString body: screen (u32), name (u32).
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&g::VENDOR_NAMES_EXT.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");

    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(9),
        RequestHeader {
            opcode: 148,
            data: g::QUERY_SERVER_STRING,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request QUERY_SERVER_STRING");

    peer.set_nonblocking(true).unwrap();
    let mut header = [0u8; 32];
    peer.read_exact(&mut header)
        .expect("reply header delivered");
    assert_eq!(header[0], 1, "byte 0 must be 1 (Reply)");
    assert_eq!(u16::from_le_bytes([header[2], header[3]]), 9, "sequence");

    // Reply layout, read off encode_string_reply (glx.rs:267-291):
    //   0      1 (Reply)      1      0 (pad)
    //   2..4   sequence       4..8   length_units = padded / 4
    //   8..12  pad1           12..16 n (string length INCLUDING NUL)
    //   16..32 pad3..pad6     then bytes + NUL + zero padding
    // Note n sits at 12, not 8 -- offset 8 is pad1. The
    // GetDrawableAttributes reply carries numAttribs at 8, which is a
    // different reply shape; do not copy that offset here.
    let n = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    assert_eq!(n as usize, "nvidia mesa".len() + 1, "n counts the NUL");

    let padded = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4;
    assert_eq!(padded, 12, "11 bytes + NUL, already 4-aligned");
    let mut tail = vec![0u8; padded];
    peer.read_exact(&mut tail).expect("reply tail delivered");
    let s = String::from_utf8_lossy(&tail);
    assert!(
        s.starts_with("nvidia mesa"),
        "arm must read state, not the VENDOR_NAMES constant; got {s:?}"
    );
}

/// Error arm 1: a naked X **pixmap** (resolvable as a drawable but not
/// a window, no GLX record) gets the extension error `GLXBadDrawable`
/// (`GLX_FIRST_ERROR + 2`) — GLXVND forwards pixmaps because they carry
/// RC_DRAWABLE, then `dixLookupWindow` fails (glxcmds.c:1873-1880).
#[test]
fn glx_get_drawable_attributes_naked_pixmap_returns_glx_bad_drawable() {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    let pixmap_xid: u32 = 0x2000;
    state.resources.create_pixmap(
        client_id,
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 32,
        },
    );

    let body = pixmap_xid.to_le_bytes().to_vec();
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: g::GET_DRAWABLE_ATTRIBUTES,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request should not hard-error");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("GLXBadDrawable error packet must be delivered");
    assert_eq!(buf[0], 0, "byte 0 must be 0 (Error class)");
    let expected_code =
        crate::nested::GLX_FIRST_ERROR + yserver_protocol::x11::glx::ERROR_GLX_BAD_DRAWABLE;
    assert_eq!(
        buf[1], expected_code,
        "expected GLXBadDrawable ({}), got {}",
        expected_code, buf[1]
    );
}

/// Error arm 2: an XID that is not a drawable at all gets **core
/// `BadDrawable` (9)** — GLXVND's XID-map lookup returns NULL and the
/// dispatch stub errors out before `DoGetDrawableAttributes` is ever
/// reached (glx/vnd_dispatch_stubs.c:456-472). The two error arms must
/// not be collapsed.
#[test]
fn glx_get_drawable_attributes_unknown_xid_returns_core_bad_drawable() {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    let unknown_xid: u32 = 0x9999;
    let body = unknown_xid.to_le_bytes().to_vec();
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: g::GET_DRAWABLE_ATTRIBUTES,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request should not hard-error");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("core BadDrawable error packet must be delivered");
    assert_eq!(buf[0], 0, "byte 0 must be 0 (Error class)");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_DRAWABLE,
        "expected core BadDrawable (9), got {}",
        buf[1]
    );
    assert_eq!(
        u16::from_le_bytes([buf[8], buf[9]]),
        u16::from(g::GET_DRAWABLE_ATTRIBUTES),
        "error must identify GLX::GetDrawableAttributes as the minor opcode"
    );
    assert_eq!(
        buf[10],
        crate::nested::GLX_MAJOR_OPCODE,
        "error must identify GLX as the major opcode"
    );
}

/// Read the `contextTag` field (bytes 8..12) out of a 32-byte
/// `MakeCurrent` reply.
fn make_current_reply_tag(buf: &[u8; 32]) -> u32 {
    u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]])
}

/// Drive a `MakeCurrent`/`MakeContextCurrent` request and return the
/// 32-byte reply. `context` is placed at the minor-specific offset.
fn drive_make_current(minor: u8, context: u32) -> [u8; 32] {
    use yserver_protocol::x11::glx as g;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Layouts (glxproto.h:225-233, :471-481), body-relative:
    //   minor 5  MakeCurrent:         drawable, context, oldContextTag
    //   minor 26 MakeContextCurrent:  oldContextTag, drawable, readdrawable, context
    let mut body = Vec::new();
    if minor == g::MAKE_CURRENT {
        body.extend_from_slice(&0x2000u32.to_le_bytes()); // drawable
        body.extend_from_slice(&context.to_le_bytes()); // context
        body.extend_from_slice(&0u32.to_le_bytes()); // oldContextTag
    } else {
        body.extend_from_slice(&0u32.to_le_bytes()); // oldContextTag
        body.extend_from_slice(&0x2000u32.to_le_bytes()); // drawable
        body.extend_from_slice(&0x2000u32.to_le_bytes()); // readdrawable
        body.extend_from_slice(&context.to_le_bytes()); // context
    }
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: minor,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request MakeCurrent");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("MakeCurrent reply delivered");
    assert_eq!(buf[0], 1, "byte 0 must be 1 (Reply)");
    buf
}

/// D6: the release form (`context == None`) must return
/// `contextTag = 0` — tag 0 is reserved by the protocol to mean "no
/// context current" (Xorg vndcmds.c:232-234, :271-273). Both minors.
#[test]
fn glx_make_current_release_returns_zero_tag() {
    use yserver_protocol::x11::glx as g;

    let buf = drive_make_current(g::MAKE_CURRENT, 0);
    assert_eq!(
        make_current_reply_tag(&buf),
        0,
        "MakeCurrent release must return contextTag = 0"
    );
}

#[test]
fn glx_make_context_current_release_returns_zero_tag() {
    use yserver_protocol::x11::glx as g;

    let buf = drive_make_current(g::MAKE_CONTEXT_CURRENT, 0);
    assert_eq!(
        make_current_reply_tag(&buf),
        0,
        "MakeContextCurrent release must return contextTag = 0"
    );
}

/// D6: a non-null context still gets a fresh non-zero tag, for both
/// minors (regression guard on the release fix).
#[test]
fn glx_make_current_non_null_context_returns_nonzero_tag() {
    use yserver_protocol::x11::glx as g;

    let buf = drive_make_current(g::MAKE_CURRENT, 0x3000);
    assert_ne!(
        make_current_reply_tag(&buf),
        0,
        "MakeCurrent with a context must return a non-zero tag"
    );
}

#[test]
fn glx_make_context_current_non_null_context_returns_nonzero_tag() {
    use yserver_protocol::x11::glx as g;

    let buf = drive_make_current(g::MAKE_CONTEXT_CURRENT, 0x3000);
    assert_ne!(
        make_current_reply_tag(&buf),
        0,
        "MakeContextCurrent with a context must return a non-zero tag"
    );
}

/// GLX Task 3.4 regression: the export ref must be released on
/// `glXDestroyPixmap` even when the client called X11 `FreePixmap` on
/// the underlying X pixmap FIRST (a common compositor ordering). The
/// release site must use the host_xid stored at acquire time, NOT a
/// re-resolution via `resources.pixmap(x_drawable)` (which is gone after
/// FreePixmap). Against the old re-resolving logic this test FAILS:
/// ReleaseGlxPixmapExport is never recorded → the export ref leaks.
#[test]
fn glx_destroy_releases_export_even_after_free_pixmap() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Register an X pixmap with a known host_xid.
    let x_pixmap_xid: u32 = 0x3000;
    let host_xid_raw: u32 = 0xdead_0002;
    state.resources.create_pixmap(
        client_id,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(x_pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 32,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(x_pixmap_xid),
        crate::backend::PixmapHandle::from_raw(host_xid_raw).unwrap(),
    ));

    let glx_xid: u32 = 0x4000_0003;

    // glXCreatePixmap — acquire.
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&0u32.to_le_bytes()); // screen
    create_body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
    create_body.extend_from_slice(&x_pixmap_xid.to_le_bytes());
    create_body.extend_from_slice(&glx_xid.to_le_bytes());
    let length_units = u32::try_from(1 + create_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::CREATE_PIXMAP,
            length_units,
        },
        &create_body,
        None,
    )
    .expect("process_request CREATE_PIXMAP");
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::AcquireGlxPixmapExport(host_xid_raw)),
        "AcquireGlxPixmapExport must be recorded"
    );

    // X11 FreePixmap on the underlying X pixmap BEFORE glXDestroyPixmap.
    // This removes the pixmap from the resource table — any subsequent
    // re-resolution of x_drawable→host_xid would fail.
    let free_body = x_pixmap_xid.to_le_bytes().to_vec();
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(2),
        RequestHeader {
            opcode: 54, // FreePixmap
            data: 0,
            length_units: 2,
        },
        &free_body,
        None,
    )
    .expect("process_request FreePixmap");
    // Sanity: the pixmap resource is really gone.
    assert!(
        state.resources.pixmap(ResourceId(x_pixmap_xid)).is_none(),
        "X pixmap must be freed by FreePixmap"
    );

    // glXDestroyPixmap — must still release using the stored host_xid.
    let destroy_body = glx_xid.to_le_bytes().to_vec();
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(3),
        RequestHeader {
            opcode: 148,
            data: x11glx::DESTROY_PIXMAP,
            length_units: 2,
        },
        &destroy_body,
        None,
    )
    .expect("process_request DESTROY_PIXMAP");

    assert!(
        !state.glx_drawables.contains_key(&glx_xid),
        "GlxDrawable must be removed after DestroyPixmap"
    );
    // THE REGRESSION ASSERTION: release must fire despite the early FreePixmap.
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::ReleaseGlxPixmapExport(host_xid_raw)),
        "ReleaseGlxPixmapExport must be recorded even after the X pixmap was \
             freed before glXDestroyPixmap (no export-ref leak)"
    );
}

/// GLX Task 3.4: `glXCreatePixmap` with a non-existent X pixmap XID
/// must reply with a GLXBadPixmap error rather than silently inserting.
#[test]
fn glx_create_pixmap_with_missing_x_pixmap_returns_glx_bad_pixmap() {
    use yserver_protocol::x11::glx as x11glx;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Use an XID that was never registered.
    let bad_x_pixmap_xid: u32 = 0x9999;
    let glx_xid: u32 = 0x4000_0002;

    let mut create_body = Vec::new();
    create_body.extend_from_slice(&0u32.to_le_bytes()); // screen
    create_body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
    create_body.extend_from_slice(&bad_x_pixmap_xid.to_le_bytes());
    create_body.extend_from_slice(&glx_xid.to_le_bytes());
    let length_units = u32::try_from(1 + create_body.len().div_ceil(4)).expect("fits");

    let outcome = process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::CREATE_PIXMAP,
            length_units,
        },
        &create_body,
        None,
    )
    .expect("process_request should not hard-error");
    assert!(matches!(outcome, RequestOutcome::Handled));

    // Nothing inserted in glx_drawables.
    assert!(
        !state.glx_drawables.contains_key(&glx_xid),
        "GlxDrawable must NOT be inserted when X pixmap does not exist"
    );

    // Error packet must be delivered: GLXBadPixmap = GLX_FIRST_ERROR + 3 = 172.
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf)
        .expect("GLXBadPixmap error packet must be delivered");
    assert_eq!(buf[0], 0, "byte 0 must be 0 (Error class)");
    let expected_code =
        crate::nested::GLX_FIRST_ERROR + yserver_protocol::x11::glx::ERROR_GLX_BAD_PIXMAP;
    assert_eq!(
        buf[1], expected_code,
        "expected GLXBadPixmap ({}), got {}",
        expected_code, buf[1]
    );
}

/// GLX Task 3.5: `VendorPrivate` with vendor_code 1330 (`BindTexImageEXT`)
/// over a valid GLXPixmap must NOT be rejected with
/// `GLXUnsupportedPrivateRequest`; it must succeed (return `Handled`
/// with no error packet delivered to the client).
///
/// Secondary assertion: an unknown vendor code still gets
/// `GLXUnsupportedPrivateRequest` (preservation of the fallthrough).
#[test]
fn bind_tex_image_ext_is_dispatched_not_rejected() {
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Register an X pixmap with a known host_xid — mirrors the
    // Task 3.4 test setup so we can get a GlxDrawable into state.
    let x_pixmap_xid: u32 = 0x2000;
    let host_xid_raw: u32 = 0xdead_1330;
    state.resources.create_pixmap(
        client_id,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: yserver_protocol::x11::ResourceId(x_pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 32,
            height: 32,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            yserver_protocol::x11::ResourceId(x_pixmap_xid),
            crate::backend::PixmapHandle::from_raw(host_xid_raw).unwrap(),
        ),
        "set_pixmap_host_xid must succeed"
    );

    // First, call GLX::CreatePixmap to insert a GlxDrawable.
    let glx_xid: u32 = 0x4000_0002;
    let fbconfig: u32 = 0x101;
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&0u32.to_le_bytes()); // screen = 0
    create_body.extend_from_slice(&fbconfig.to_le_bytes());
    create_body.extend_from_slice(&x_pixmap_xid.to_le_bytes());
    create_body.extend_from_slice(&glx_xid.to_le_bytes());
    let length_units = u32::try_from(1 + create_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::CREATE_PIXMAP,
            length_units,
        },
        &create_body,
        None,
    )
    .expect("process_request CREATE_PIXMAP");

    // Build a VendorPrivate body for BindTexImageEXT (1330):
    //   [vendor_code=1330][context_tag=0][glx_drawable=glx_xid][buffer=GLX_FRONT_LEFT_EXT]
    let mut bind_body = Vec::new();
    bind_body.extend_from_slice(&x11glx::VENDOR_CODE_BIND_TEX_IMAGE.to_le_bytes());
    bind_body.extend_from_slice(&0u32.to_le_bytes()); // context_tag
    bind_body.extend_from_slice(&glx_xid.to_le_bytes());
    bind_body.extend_from_slice(&x11glx::GLX_FRONT_LEFT_EXT.to_le_bytes());
    let length_units = u32::try_from(1 + bind_body.len().div_ceil(4)).expect("fits");
    let outcome = process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(2),
        RequestHeader {
            opcode: 148,
            data: x11glx::VENDOR_PRIVATE, // minor = 16
            length_units,
        },
        &bind_body,
        None,
    )
    .expect("process_request VENDOR_PRIVATE BindTexImageEXT must not hard-error");
    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "BindTexImageEXT must return Handled, got {outcome:?}"
    );

    // No error packet must be delivered to the client.
    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    match peer.read(&mut buf) {
        Ok(0) | Err(_) => { /* no data written — correct */ }
        Ok(n) => {
            assert_ne!(
                buf[0], 0,
                "BindTexImageEXT must not deliver an X11 error packet; \
                     got {n} bytes, first byte={} (0=Error)",
                buf[0]
            );
        }
    }
    peer.set_nonblocking(false).unwrap();

    // The unsupported-private rejection must still fire for unknown codes.
    let mut unknown_body = Vec::new();
    unknown_body.extend_from_slice(&9999u32.to_le_bytes()); // unknown vendor code
    unknown_body.extend_from_slice(&0u32.to_le_bytes());
    unknown_body.extend_from_slice(&0u32.to_le_bytes());
    unknown_body.extend_from_slice(&0u32.to_le_bytes());
    let length_units = u32::try_from(1 + unknown_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(3),
        RequestHeader {
            opcode: 148,
            data: x11glx::VENDOR_PRIVATE,
            length_units,
        },
        &unknown_body,
        None,
    )
    .expect("process_request VENDOR_PRIVATE unknown must not hard-error");

    // Error packet for the unknown vendor code.
    peer.set_nonblocking(true).unwrap();
    let mut buf2 = [0u8; 32];
    peer.read_exact(&mut buf2)
        .expect("GLXUnsupportedPrivateRequest error packet must be delivered");
    assert_eq!(buf2[0], 0, "byte 0 must be 0 (Error class)");
    let expected_unsupported = crate::nested::GLX_FIRST_ERROR
        + yserver_protocol::x11::glx::ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST;
    assert_eq!(
        buf2[1], expected_unsupported,
        "expected GLXUnsupportedPrivateRequest ({expected_unsupported}), got {}",
        buf2[1]
    );
    peer.set_nonblocking(false).unwrap();
}

/// GLX Task 3.5 regression (codex review): `BindTexImageEXT` /
/// `ReleaseTexImageEXT` must be DECOUPLED from the GLXPixmap LIFETIME
/// refcount (`glx_refs`).
///
/// Bind uses the promote-only hook (`promote_pixmap_exportable`),
/// idempotently, taking NO lifetime ref — so repeated rebinds (the
/// normal TFP per-frame pattern) do not grow `glx_refs` and leak the
/// backing. Release is a lifetime no-op — it must NOT call
/// `release_glx_pixmap_export`, or a release without a matching bind
/// would tear the backing down while the GLXPixmap is still alive.
///
/// Against the OLD acquire/release-based logic this test FAILS: the
/// three binds would record three `AcquireGlxPixmapExport` (and no
/// `PromotePixmapExportable`), and the release would record a
/// `ReleaseGlxPixmapExport`.
#[test]
fn bind_release_tex_image_do_not_touch_lifetime_refcount() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Register an X pixmap with a known host_xid.
    let x_pixmap_xid: u32 = 0x2100;
    let host_xid_raw: u32 = 0xdead_1331;
    state.resources.create_pixmap(
        client_id,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: yserver_protocol::x11::ResourceId(x_pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 32,
            height: 32,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            yserver_protocol::x11::ResourceId(x_pixmap_xid),
            crate::backend::PixmapHandle::from_raw(host_xid_raw).unwrap(),
        ),
        "set_pixmap_host_xid must succeed"
    );

    // glXCreatePixmap inserts the GlxDrawable and takes the ONE lifetime
    // acquire ref (this is the legitimate AcquireGlxPixmapExport).
    let glx_xid: u32 = 0x4000_0003;
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&0u32.to_le_bytes()); // screen
    create_body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
    create_body.extend_from_slice(&x_pixmap_xid.to_le_bytes());
    create_body.extend_from_slice(&glx_xid.to_le_bytes());
    let length_units = u32::try_from(1 + create_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::CREATE_PIXMAP,
            length_units,
        },
        &create_body,
        None,
    )
    .expect("process_request CREATE_PIXMAP");

    // Exactly one acquire (the lifetime ref) so far; no promote yet.
    let after_create = backend.calls();
    assert_eq!(
        after_create
            .iter()
            .filter(|c| matches!(c, RecordedCall::AcquireGlxPixmapExport(_)))
            .count(),
        1,
        "glXCreatePixmap must take exactly one lifetime acquire ref"
    );
    let acquires_before_binds = after_create
        .iter()
        .filter(|c| matches!(c, RecordedCall::AcquireGlxPixmapExport(_)))
        .count();
    let releases_before_binds = after_create
        .iter()
        .filter(|c| matches!(c, RecordedCall::ReleaseGlxPixmapExport(_)))
        .count();

    // Build a BindTexImageEXT VendorPrivate body.
    let mut bind_body = Vec::new();
    bind_body.extend_from_slice(&x11glx::VENDOR_CODE_BIND_TEX_IMAGE.to_le_bytes());
    bind_body.extend_from_slice(&0u32.to_le_bytes()); // context_tag
    bind_body.extend_from_slice(&glx_xid.to_le_bytes());
    bind_body.extend_from_slice(&x11glx::GLX_FRONT_LEFT_EXT.to_le_bytes());
    let bind_units = u32::try_from(1 + bind_body.len().div_ceil(4)).expect("fits");

    // Bind THREE times (compositor per-frame / implicit-rebind pattern).
    for seq in 2u16..=4 {
        process_request(
            &mut state,
            &mut backend,
            client_id,
            SequenceNumber(seq),
            RequestHeader {
                opcode: 148,
                data: x11glx::VENDOR_PRIVATE,
                length_units: bind_units,
            },
            &bind_body,
            None,
        )
        .expect("process_request BindTexImageEXT");
    }

    let after_binds = backend.calls();
    // Each bind recorded a PromotePixmapExportable (the lightweight hook).
    assert_eq!(
        after_binds
            .iter()
            .filter(|c| matches!(
                c,
                RecordedCall::PromotePixmapExportable(h) if *h == host_xid_raw
            ))
            .count(),
        3,
        "each BindTexImageEXT must record PromotePixmapExportable, not Acquire"
    );
    // The binds must NOT have taken any extra lifetime ref.
    assert_eq!(
        after_binds
            .iter()
            .filter(|c| matches!(c, RecordedCall::AcquireGlxPixmapExport(_)))
            .count(),
        acquires_before_binds,
        "BindTexImageEXT must NOT take a lifetime acquire ref (no leak on rebind)"
    );
    assert_eq!(
        after_binds
            .iter()
            .filter(|c| matches!(c, RecordedCall::ReleaseGlxPixmapExport(_)))
            .count(),
        releases_before_binds,
        "BindTexImageEXT must NOT change the lifetime release count"
    );

    // Now ReleaseTexImageEXT — must be a lifetime no-op.
    let mut rel_body = Vec::new();
    rel_body.extend_from_slice(&x11glx::VENDOR_CODE_RELEASE_TEX_IMAGE.to_le_bytes());
    rel_body.extend_from_slice(&0u32.to_le_bytes()); // context_tag
    rel_body.extend_from_slice(&glx_xid.to_le_bytes());
    rel_body.extend_from_slice(&x11glx::GLX_FRONT_LEFT_EXT.to_le_bytes());
    let rel_units = u32::try_from(1 + rel_body.len().div_ceil(4)).expect("fits");
    process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(5),
        RequestHeader {
            opcode: 148,
            data: x11glx::VENDOR_PRIVATE,
            length_units: rel_units,
        },
        &rel_body,
        None,
    )
    .expect("process_request ReleaseTexImageEXT");

    // THE REGRESSION ASSERTION: release must NOT have dropped the
    // lifetime ref — the backing survives until glXDestroyPixmap.
    let after_release = backend.calls();
    assert_eq!(
        after_release
            .iter()
            .filter(|c| matches!(c, RecordedCall::ReleaseGlxPixmapExport(_)))
            .count(),
        releases_before_binds,
        "ReleaseTexImageEXT must NOT call release_glx_pixmap_export \
             (lifetime ref untouched; backing survives until glXDestroyPixmap)"
    );
}

// ── GLX_SGIX_fbconfig dispatch tests ─────────────────────────────────────

/// Build a VendorPrivateWithReply body for `GetFBConfigsSGIX` (vendor
/// code 65540):
///   [0..4]  vendorCode
///   [4..8]  pad1 = 0
///   [8..12] screen = 0
fn build_get_fb_configs_sgix_body(screen: u32) -> Vec<u8> {
    use yserver_protocol::x11::glx as x11glx;
    let mut body = Vec::new();
    body.extend_from_slice(&x11glx::VENDOR_CODE_GET_FB_CONFIGS_SGIX.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // pad1
    body.extend_from_slice(&screen.to_le_bytes());
    body
}

/// `VendorPrivateWithReply` with vendor_code `GetFBConfigsSGIX` (65540)
/// must NOT be rejected with `GLXUnsupportedPrivateRequest`; it must
/// return a non-empty GetFBConfigs reply (real configs, not a stub).
#[test]
fn get_fb_configs_sgix_is_dispatched_not_rejected() {
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    let body = build_get_fb_configs_sgix_body(0);
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");

    let outcome = process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::VENDOR_PRIVATE_WITH_REPLY,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request must not hard-error");

    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "GetFBConfigsSGIX must return Handled, got {outcome:?}"
    );

    // Must NOT deliver an error packet.
    peer.set_nonblocking(true).unwrap();
    let bytes = read_all_available(&mut peer);
    assert!(
        !bytes.is_empty(),
        "GetFBConfigsSGIX must deliver a reply packet"
    );
    assert_ne!(
        bytes[0], 0,
        "byte 0 must not be 0 (Error); got {bytes:02x?}"
    );
    // byte 0 = 1 (Reply), byte 8..12 = num_FB_configs > 0.
    assert_eq!(bytes[0], 1, "byte 0 must be Reply type");
    let num_configs = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert!(
        num_configs > 0,
        "GetFBConfigsSGIX must return at least one FBConfig, got {num_configs}"
    );
    peer.set_nonblocking(false).unwrap();
}

/// `VendorPrivate` with vendor_code `CreateContextWithConfigSGIX`
/// (65541) must insert a `GlxContext` entry (no reply, no error).
#[test]
fn create_context_with_config_sgix_inserts_context() {
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    let context_xid: u32 = 0xABCD_0001;
    let fbconfig: u32 = 0x101;

    // Build CreateContextWithConfigSGIX body:
    //   [0..4]  vendorCode = 65541
    //   [4..8]  pad1 = 0
    //   [8..12] context = context_xid
    //   [12..16] fbconfig = 0x101
    //   [16..20] screen = 0
    //   [20..24] renderType = 1
    //   [24..28] shareList = 0
    //   [28] isDirect = 1
    //   [29..32] reserved = 0
    let mut body = Vec::new();
    body.extend_from_slice(&x11glx::VENDOR_CODE_CREATE_CONTEXT_WITH_CONFIG_SGIX.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // pad1
    body.extend_from_slice(&context_xid.to_le_bytes());
    body.extend_from_slice(&fbconfig.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // screen
    body.extend_from_slice(&1u32.to_le_bytes()); // renderType
    body.extend_from_slice(&0u32.to_le_bytes()); // shareList
    body.push(1u8); // isDirect
    body.push(0u8); // reserved1
    body.extend_from_slice(&0u16.to_le_bytes()); // reserved2

    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");

    let outcome = process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::VENDOR_PRIVATE,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request must not hard-error");

    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "CreateContextWithConfigSGIX must return Handled, got {outcome:?}"
    );

    // GlxContext must be inserted.
    assert!(
        state.glx_contexts.contains_key(&context_xid),
        "GlxContext must be inserted for xid=0x{context_xid:x}"
    );
    let ctx = &state.glx_contexts[&context_xid];
    assert_eq!(ctx.fbconfig, fbconfig, "fbconfig must be recorded");

    // No error packet.
    peer.set_nonblocking(true).unwrap();
    let bytes = read_all_available(&mut peer);
    assert!(
        bytes.is_empty(),
        "CreateContextWithConfigSGIX must not deliver an error; got {bytes:02x?}"
    );
    peer.set_nonblocking(false).unwrap();
}

/// `VendorPrivate` with vendor_code `CreateGLXPixmapWithConfigSGIX`
/// (65542) must insert a `GlxDrawable` entry and call
/// `acquire_glx_pixmap_export` — same semantics as `CREATE_PIXMAP`.
#[test]
fn create_glx_pixmap_with_config_sgix_inserts_drawable_and_acquires() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::glx as x11glx;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_id = ClientId(1);

    // Register an X pixmap with a known host_xid.
    let x_pixmap_xid: u32 = 0x3000;
    let host_xid_raw: u32 = 0xdead_5542;
    state.resources.create_pixmap(
        client_id,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: yserver_protocol::x11::ResourceId(x_pixmap_xid),
            drawable: ROOT_WINDOW,
            width: 64,
            height: 64,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            yserver_protocol::x11::ResourceId(x_pixmap_xid),
            crate::backend::PixmapHandle::from_raw(host_xid_raw).unwrap(),
        ),
        "set_pixmap_host_xid must succeed"
    );

    let glx_xid: u32 = 0x5000_0010;
    let fbconfig: u32 = 0x101;

    // Build CreateGLXPixmapWithConfigSGIX body:
    //   [0..4]  vendorCode = 65542
    //   [4..8]  pad1 = 0
    //   [8..12]  screen = 0
    //   [12..16] fbconfig = 0x101
    //   [16..20] pixmap = x_pixmap_xid
    //   [20..24] glxpixmap = glx_xid
    let mut body = Vec::new();
    body.extend_from_slice(&x11glx::VENDOR_CODE_CREATE_GLX_PIXMAP_WITH_CONFIG_SGIX.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // pad1
    body.extend_from_slice(&0u32.to_le_bytes()); // screen
    body.extend_from_slice(&fbconfig.to_le_bytes());
    body.extend_from_slice(&x_pixmap_xid.to_le_bytes());
    body.extend_from_slice(&glx_xid.to_le_bytes());

    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("fits");

    let outcome = process_request(
        &mut state,
        &mut backend,
        client_id,
        SequenceNumber(1),
        RequestHeader {
            opcode: 148,
            data: x11glx::VENDOR_PRIVATE,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request must not hard-error");

    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "CreateGLXPixmapWithConfigSGIX must return Handled, got {outcome:?}"
    );

    // GlxDrawable must be inserted.
    assert!(
        state.glx_drawables.contains_key(&glx_xid),
        "GlxDrawable must be inserted for glx_xid=0x{glx_xid:x}"
    );
    let drawable = &state.glx_drawables[&glx_xid];
    assert_eq!(drawable.x_drawable, x_pixmap_xid, "x_drawable must be set");
    assert_eq!(drawable.fbconfig, fbconfig, "fbconfig must be recorded");
    assert_eq!(
        drawable.glx_export_host_xid,
        Some(host_xid_raw),
        "host_xid must be stored for later release"
    );

    // acquire_glx_pixmap_export must have been called (same as CREATE_PIXMAP).
    assert!(
        backend
            .calls()
            .contains(&RecordedCall::AcquireGlxPixmapExport(host_xid_raw)),
        "AcquireGlxPixmapExport must be recorded"
    );

    // No error packet.
    peer.set_nonblocking(true).unwrap();
    let bytes = read_all_available(&mut peer);
    assert!(
        bytes.is_empty(),
        "CreateGLXPixmapWithConfigSGIX must not deliver an error; got {bytes:02x?}"
    );
    peer.set_nonblocking(false).unwrap();
}

/// After both dispatch and extension-string advertisement are live,
/// the computed GLX extension string must advertise
/// `GLX_SGIX_fbconfig` as a whole token, under both TFP settings.
///
/// This test used to re-implement `glx_extension_string`'s body and
/// assert against its own copy, so it would have passed even if the
/// builder were deleted. It calls the real function now.
#[test]
fn glx_extension_string_contains_sgix_fbconfig() {
    use yserver_protocol::x11::glx as x11glx;
    for tfp_supported in [false, true] {
        let s = glx_extension_string(tfp_supported);
        assert!(
            s.split_whitespace()
                .any(|token| token == x11glx::SGIX_FBCONFIG_EXTENSION),
            "glx_extension_string must advertise GLX_SGIX_fbconfig as a \
                 whole token; tfp_supported={tfp_supported} produced {s:?}"
        );
    }
}
