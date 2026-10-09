use super::*;

/// Build the FBConfig list returned by `GetFBConfigs`. We synthesise
/// from each X visual (depth-24 RGB and depth-32 ARGB), double buffered,
/// plus one visual-less, single-buffered pixmap configuration for
/// QtWebEngine's native DMA-BUF import.  All share depth=24 stencil=8 (the
/// universal default for OpenGL apps).  The two visual-backed configurations
/// advertise GLX_PBUFFER_BIT so Chromium/ANGLE can allocate its offscreen
/// surface.
/// Resolve the attribute list for a `GetDrawableAttributes` reply,
/// mirroring Xorg's `DoGetDrawableAttributes` (glxcmds.c:1863-1914).
/// With no GLX record (a naked X window queried directly, the GLX 1.2
/// pattern) the `pGlxDraw` block is skipped entirely — `GLX_FBCONFIG_ID`,
/// `GLX_TEXTURE_TARGET_EXT` and `GLX_EVENT_MASK` are absent, not zero.
pub(super) fn drawable_attributes_for(state: &ServerState, xid: u32) -> Vec<(u32, u32)> {
    use yserver_protocol::x11::glx as g;
    let drawable = state.glx_drawables.get(&xid);
    // Resolve drawable geometry. For a pbuffer the size lives in the
    // GlxDrawable record (from CreatePbuffer); otherwise read the real
    // geometry of the backing X drawable from the resource store. Mesa's
    // loader_dri3 reads GLX_WIDTH/GLX_HEIGHT here to size the buffer;
    // without them it gets 0×0 and fails with "failed to create drawable".
    // Xorg reports the same from pDraw->width/height (glxcmds.c:1891).
    // A 0×0 pbuffer is treated as NOT a pbuffer here (matches the old
    // size-keyed behaviour): its geometry falls through to the backing
    // pixmap, which CREATE_PBUFFER clamps to max(1), so it reports 1×1.
    let is_pbuffer = matches!(
        drawable,
        Some(d) if d.kind == crate::server::GlxDrawableKind::Pbuffer
            && (d.width != 0 || d.height != 0)
    );
    // A GLXWindow/GLXPixmap XID is a fresh client-allocated id with no X
    // resource behind it — its geometry lives on the *backing* X drawable
    // recorded at create time. Looking up the GLX XID itself always missed
    // and reported 0×0. Xorg reads pGlxDraw->pDraw->width/height
    // (glxcmds.c:1891).
    let geometry_xid = match drawable {
        Some(d) if !is_pbuffer => d.x_drawable,
        _ => xid,
    };
    let (width, height) = match drawable {
        Some(d) if is_pbuffer => (d.width, d.height),
        _ => state
            .resources
            .window(ResourceId(geometry_xid))
            .map(|w| (u32::from(w.width), u32::from(w.height)))
            .or_else(|| {
                state
                    .resources
                    .pixmap(ResourceId(geometry_xid))
                    .map(|p| (u32::from(p.width), u32::from(p.height)))
            })
            .unwrap_or((0, 0)),
    };
    // Xorg's exact attribute set and order (glxcmds.c:1889-1914).
    let mut attribs: Vec<(u32, u32)> = Vec::with_capacity(9);
    attribs.push((g::GLX_Y_INVERTED_EXT, 0));
    attribs.push((g::GLX_WIDTH, width));
    attribs.push((g::GLX_HEIGHT, height));
    attribs.push((g::GLX_SCREEN, 0));
    if let Some(d) = drawable {
        attribs.push((g::GLX_TEXTURE_TARGET_EXT, d.texture_target));
        attribs.push((g::GLX_EVENT_MASK, d.event_mask));
        attribs.push((g::GLX_FBCONFIG_ID, d.fbconfig));
        if d.kind == crate::server::GlxDrawableKind::Pbuffer {
            attribs.push((g::GLX_PRESERVED_CONTENTS, 1));
        }
        if d.kind == crate::server::GlxDrawableKind::Window {
            attribs.push((g::GLX_STEREO_TREE_EXT, 0));
        }
    }
    // GLX_EXT_get_drawable_type — always last; Xorg's no-record
    // fallthrough is GLX_WINDOW_BIT (glxcmds.c:1908-1910).
    let drawable_type = match drawable.map(|d| d.kind) {
        Some(crate::server::GlxDrawableKind::Pixmap) => g::GLX_PIXMAP_BIT,
        Some(crate::server::GlxDrawableKind::Pbuffer) => g::GLX_PBUFFER_BIT,
        Some(crate::server::GlxDrawableKind::Window) | None => g::GLX_WINDOW_BIT,
    };
    attribs.push((g::GLX_DRAWABLE_TYPE, drawable_type));
    attribs
}

/// GLX 1.2 visual configs in the legacy untagged form Mesa parses
/// via `__glXInitializeVisualConfigFromTags(!tagged_only)`. Mirrors
/// the visuals our setup-reply advertises (ROOT_VISUAL depth-24
/// TrueColor RGB, ARGB_VISUAL depth-32 TrueColor RGBA).
///
/// An X visual describes one GLX visual configuration. Do not publish the
/// same visual ID once as double-buffered and again as single-buffered: Mesa
/// cannot preserve that ambiguity when a context and window are selected
/// independently. ANGLE selected yserver's single-buffered config for its
/// context/pbuffer but the double-buffered config carrying the same visual for
/// its windows. Mesa then allocated a fake front buffer for every window. Xorg
/// gives those configurations distinct X visuals; until yserver exposes more
/// setup visuals, associate only the double-buffered configuration with each
/// real visual.
pub(super) fn synthesise_glx_visual_configs() -> Vec<yserver_protocol::x11::glx::VisualConfig> {
    use yserver_protocol::x11::glx::VisualConfig;
    // X11 visual class 4 == TrueColor, matching the class we put in the
    // setup-reply visual list.
    const TRUE_COLOR: u32 = 4;
    let mut out: Vec<VisualConfig> = Vec::with_capacity(3);
    for &(visual_id, alpha_bits, rgb_bits, stencil_bits) in &[
        // Must match the client driver's configs (see synthesise_glx_fb_configs):
        // radeonsi advertises alpha-8 / 32-bit-buffer for both depths.
        (0x102_u32, 8_u32, 32_u32, 8_u32), // ROOT_VISUAL — TrueColor (GL alpha 8)
        (0x103_u32, 8_u32, 32_u32, 8_u32), // ARGB_VISUAL — TrueColor RGBA
        (crate::resources::GLMARK_VISUAL.0, 8_u32, 32_u32, 0_u32),
    ] {
        out.push(VisualConfig {
            visual_id,
            visual_class: TRUE_COLOR,
            rgba: true,
            red_bits: 8,
            green_bits: 8,
            blue_bits: 8,
            alpha_bits,
            double_buffer: true,
            stereo: false,
            rgb_bits,
            depth_bits: 24,
            stencil_bits,
            aux_buffers: 0,
            level: 0,
        });
    }
    out
}

/// Build the GLX extension string.  The base extensions are always
/// present; `GLX_EXT_texture_from_pixmap` is appended only when the
/// backend confirmed at init that it can export a BGRA8 dma-buf.
/// `GLX_SGIX_fbconfig` is always appended: `GetFBConfigsSGIX`,
/// `CreateContextWithConfigSGIX` and `CreateGLXPixmapWithConfigSGIX`
/// are fully dispatched (VendorPrivate arms) — advertise-after-implement.
///
/// **Every chunk is space-terminated, so the string ends with `' '`.**
/// This mirrors Xorg, which writes `' '` then `'\0'` after each enabled
/// extension (`glx/extension_string.c:144-145`). It is not cosmetic:
/// `libGLX_nvidia` loses the final token of an unterminated list — it
/// silently dropped the `GLX_SGIX_fbconfig` we advertise — and also
/// withholds `GLX_ARB_get_proc_address` from the client extension
/// string, which aborts libepoxy and crashes `kwin_x11`. Appending via
/// this closure keeps the invariant true for any extension added later.
pub(super) fn glx_extension_string(tfp_supported: bool) -> String {
    let mut s = String::new();
    let mut push = |chunk: &str| {
        s.push_str(chunk);
        s.push(' ');
    };
    push(yserver_protocol::x11::glx::SERVER_EXTENSIONS);
    push(yserver_protocol::x11::glx::SGIX_FBCONFIG_EXTENSION);
    if tfp_supported {
        push(yserver_protocol::x11::glx::TFP_EXTENSION);
    }
    s
}

pub(super) fn synthesise_glx_fb_configs(tfp_supported: bool) -> Vec<Vec<(u32, u32)>> {
    use yserver_protocol::x11::glx as g;
    let mut out = Vec::with_capacity(4);
    let depth = 24;
    for &(visual_id, fbconfig_id, alpha_size, total_buffer_size, stencil) in &[
        // (X visual id, FBConfig id, alpha bits, total color buffer bits)
        //
        // These MUST match the client driver's __DRIconfig attributes
        // exactly, or mesa's driConfigEqual rejects them all and GLX dri3
        // screen creation fails with "No matching fbConfigs or visuals
        // found" (no direct GLX → no TFP). radeonsi advertises BOTH its
        // depth-24 and depth-32 TrueColor configs as 8-bit-alpha,
        // 32-bit-buffer (the GL backbuffer is BGRA8 regardless of the X
        // visual's opacity), so we mirror that for both. The depth-24 X
        // visual stays opaque on screen; the alpha is GL-side only.
        (0x102_u32, 0x101_u32, 8_u32, 32_u32, 8_u32), // ROOT_VISUAL — TrueColor
        // Preserve the IDs of the two double-buffered configurations while
        // dropping the ambiguous single-buffered 0x102 and 0x104 entries.
        (0x103_u32, 0x103_u32, 8_u32, 32_u32, 8_u32), // ARGB_VISUAL — TrueColor RGBA
        (
            crate::resources::GLMARK_VISUAL.0,
            0x105_u32,
            8_u32,
            32_u32,
            0_u32,
        ),
    ] {
        let mut config = vec![
            (g::GLX_VISUAL_ID, visual_id),
            (g::GLX_FBCONFIG_ID, fbconfig_id),
            (g::GLX_X_VISUAL_TYPE, g::GLX_TRUE_COLOR),
            // PBUFFER_BIT is load-bearing for Chromium/ANGLE: it allocates
            // its offscreen GL surface as a pbuffer and filters configs on
            // this bit. Without it ANGLE's HW GL init fails and Chromium
            // drops to SwiftShader (no WebGL accel → no Maps 3D). Firefox
            // uses window surfaces and is unaffected either way. (#96)
            (
                g::GLX_DRAWABLE_TYPE,
                g::GLX_WINDOW_BIT | g::GLX_PIXMAP_BIT | g::GLX_PBUFFER_BIT,
            ),
            (g::GLX_RENDER_TYPE, g::GLX_RGBA_BIT),
            (g::GLX_X_RENDERABLE, 1),
            (g::GLX_BUFFER_SIZE, total_buffer_size),
            (g::GLX_LEVEL, 0),
            (g::GLX_DOUBLEBUFFER, 1),
            (g::GLX_STEREO, 0),
            (g::GLX_AUX_BUFFERS, 0),
            (g::GLX_RED_SIZE, 8),
            (g::GLX_GREEN_SIZE, 8),
            (g::GLX_BLUE_SIZE, 8),
            (g::GLX_ALPHA_SIZE, alpha_size),
            (g::GLX_DEPTH_SIZE, depth),
            (g::GLX_STENCIL_SIZE, stencil),
            (g::GLX_ACCUM_RED_SIZE, 0),
            (g::GLX_ACCUM_GREEN_SIZE, 0),
            (g::GLX_ACCUM_BLUE_SIZE, 0),
            (g::GLX_ACCUM_ALPHA_SIZE, 0),
            (g::GLX_CONFIG_CAVEAT, g::GLX_NONE),
            (g::GLX_TRANSPARENT_TYPE, g::GLX_NONE),
            (g::GLX_SAMPLE_BUFFERS, 0),
            (g::GLX_SAMPLES, 0),
            // Pbuffer size caps — required alongside PBUFFER_BIT so ANGLE's
            // glXGetFBConfigAttrib(GLX_MAX_PBUFFER_*) validation passes.
            // 16384 comfortably exceeds any browser offscreen surface
            // (ANGLE's init pbuffer is 1×1). Not compared by driConfigEqual.
            (g::GLX_MAX_PBUFFER_WIDTH, 16384),
            (g::GLX_MAX_PBUFFER_HEIGHT, 16384),
            (g::GLX_MAX_PBUFFER_PIXELS, 16384 * 16384),
        ];
        // Append bind-to-texture pairs in Xorg's exact reply order
        // (glxcmds.c:1094-1100 / glxdricommon.c:165) when TFP is supported.
        //
        // These attributes are scalar-compared by mesa's driConfigEqual
        // against the client driver's __DRIconfig (attribMap in
        // src/glx/dri_common.c), so they MUST match what radeonsi
        // advertises or the whole config is rejected and dri3 screen
        // creation fails ("No matching fbConfigs or visuals found").
        // HW-verified on bee (radeonsi via Xwayland): every TFP config
        // reports BIND_RGB=1, BIND_RGBA=1, and BIND_TARGETS=GLX_DONT_CARE.
        // So advertise RGBA=1 for BOTH depths (depth-24 windows are
        // opaque; the sample_view forces α=1, so an RGBA bind reads as
        // opaque — correct) and DONT_CARE for targets (matches any
        // driver target bitmask, as Xwayland itself does).
        if tfp_supported {
            config.push((g::GLX_BIND_TO_TEXTURE_RGB_EXT, 1));
            config.push((g::GLX_BIND_TO_TEXTURE_RGBA_EXT, 1));
            // MIPMAP: not backed, but pair MUST be present for contract.
            config.push((g::GLX_BIND_TO_MIPMAP_TEXTURE_EXT, 0));
            config.push((g::GLX_BIND_TO_TEXTURE_TARGETS_EXT, g::GLX_DONT_CARE));
            // Y_INVERTED in FBConfig = GLX_DONT_CARE (differs from drawable-attributes
            // where it is 0/GL_FALSE — do not confuse them; glxcmds.c:1093).
            config.push((g::GLX_Y_INVERTED_EXT, g::GLX_DONT_CARE));
        }
        out.push(config);
    }
    // QtWebEngine's GLXHelper chooses a single-buffered RGBA pixmap config
    // before importing a native DMA-BUF through DRI3 (#152).  Do not attach it
    // to either real visual: #96 showed that Mesa can then pair the
    // single-buffered config with a double-buffered window of the same visual,
    // making it allocate a fake front buffer.  A visual-less, pixmap-only
    // config is sufficient for GLXPixmap and cannot be selected for windows
    // or pbuffers.  Keep the property count uniform: GetFBConfigs encodes one
    // count for the whole reply.
    let mut native_pixmap = out[0].clone();
    for (attribute, value) in &mut native_pixmap {
        match *attribute {
            g::GLX_VISUAL_ID => *value = 0,
            g::GLX_FBCONFIG_ID => *value = 0x104,
            g::GLX_X_VISUAL_TYPE => *value = g::GLX_NONE,
            g::GLX_DRAWABLE_TYPE => *value = g::GLX_PIXMAP_BIT,
            g::GLX_X_RENDERABLE => *value = 0,
            g::GLX_DOUBLEBUFFER => *value = 0,
            _ => {}
        }
    }
    out.push(native_pixmap);
    out
}

pub(super) fn handle_glx_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, glx as x11glx};
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    match minor {
        x11glx::QUERY_VERSION => {
            let (cmaj, cmin) = x11glx::parse_query_version(body).unwrap_or((0, 0));
            let major = cmaj.min(x11glx::MAJOR_VERSION);
            let minor = cmin.min(x11glx::MINOR_VERSION);
            debug!(
                "client {} #{} GLX::QueryVersion client={cmaj}.{cmin} -> {major}.{minor}",
                client_id.0, sequence.0
            );
            let reply = x11glx::encode_query_version_reply(byte_order, sequence, major, minor);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::QUERY_SERVER_STRING => {
            let req = x11glx::parse_query_server_string(body);
            let name = req.map_or(0, |r| r.name);
            // Build the extension string lazily so we only pay the
            // allocation cost when the client actually asks for it.
            let ext_string;
            let s: &str = match name {
                x11glx::STRING_VENDOR => "yserver",
                x11glx::STRING_VERSION => "1.4",
                x11glx::STRING_EXTENSIONS => {
                    ext_string = glx_extension_string(state.glx_tfp_supported);
                    &ext_string
                }
                // libglvnd vendor-neutral dispatch: tells the client which
                // libGLX_<vendor>.so drives this screen. Resolved once at
                // startup from the render driver
                // (`BackendCapabilities::from_backend`); every non-NVIDIA
                // driver keeps `VENDOR_NAMES` ("mesa"), which is what stops
                // libglvnd from falling back to a vendor that resolves to
                // nothing on Asahi → NULL glXQueryExtensionsString → cogl
                // SIGSEGV. Only queried because we advertise
                // GLX_EXT_libglvnd.
                x11glx::VENDOR_NAMES_EXT => &state.glx_vendor_names,
                _ => "",
            };
            debug!(
                "client {} #{} GLX::QueryServerString name={name:#x} -> {s:?}",
                client_id.0, sequence.0
            );
            let reply = x11glx::encode_string_reply(byte_order, sequence, s);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::QUERY_EXTENSIONS_STRING => {
            // Per design §3.5 — list of GLX extensions Mesa probes for.
            // Same list as the server string (opcode 19) — see
            // `x11glx::SERVER_EXTENSIONS`.
            let ext_string = glx_extension_string(state.glx_tfp_supported);
            let reply = x11glx::encode_string_reply(byte_order, sequence, &ext_string);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::CLIENT_INFO | x11glx::SET_CLIENT_INFO_ARB | x11glx::SET_CLIENT_INFO_2_ARB => {
            // Drop on the floor — Mesa sends one of these on connect
            // carrying its GL/GLX/GLES versions; we don't act on them.
            debug!(
                "client {} #{} GLX::ClientInfo* minor={} body_len={}",
                client_id.0,
                sequence.0,
                minor,
                body.len()
            );
        }
        x11glx::IS_DIRECT => {
            // Xorg answers the context's recorded isDirect and
            // GLXBadContext for an XID that is not a context
            // (glxcmds.c:702-726, glx/vnd_dispatch_stubs.c:509-525). Mesa's
            // glXImportContextEXT sends this first and returns NULL for a
            // direct context without ever sending QueryContext.
            let context = match glx_request_context(state, body) {
                Ok(context) => context,
                Err((code, value)) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        code,
                        value,
                        u16::from(minor),
                        crate::nested::GLX_MAJOR_OPCODE,
                    );
                }
            };
            let is_direct = state.glx_contexts[&context].is_direct;
            let reply = x11glx::encode_is_direct_reply(byte_order, sequence, is_direct);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::GET_FB_CONFIGS => {
            // Synthesise FBConfigs from each X visual × singleBuf /
            // doubleBuf. Mesa picks one matching the app's request
            // via glXChooseFBConfig.
            let configs = synthesise_glx_fb_configs(state.glx_tfp_supported);
            let config_refs: Vec<&[(u32, u32)]> = configs.iter().map(|c| c.as_slice()).collect();
            let reply = x11glx::encode_get_fb_configs_reply(byte_order, sequence, &config_refs);
            debug!(
                "client {} #{} GLX::GetFBConfigs -> {} configs × {} props",
                client_id.0,
                sequence.0,
                config_refs.len(),
                config_refs.first().map_or(0, |c| c.len()),
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::GET_VISUAL_CONFIGS => {
            // Synthesise GLX visual configs from the X visuals our
            // setup-reply advertises. Mesa's `glx_screen_init` calls
            // `getVisualConfigs` *before* `getFBConfigs` and bails the
            // entire DRI3 screen creation if we hand back zero
            // visuals — even though FBConfigs would otherwise carry
            // the same info. So we always emit at least one visual.
            let visuals = synthesise_glx_visual_configs();
            let reply = x11glx::encode_get_visual_configs_reply(byte_order, sequence, &visuals);
            debug!(
                "client {} #{} GLX::GetVisualConfigs -> {} visuals",
                client_id.0,
                sequence.0,
                visuals.len(),
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::CREATE_CONTEXT
        | x11glx::CREATE_NEW_CONTEXT
        | x11glx::CREATE_CONTEXT_ATTRIBS_ARB => {
            // Allocate a GlxContext resource keyed by the client-chosen
            // XID. We never execute server-side GL; the recorded config,
            // share list, render type and isDirect flag are what
            // QueryContext / IsDirect report back (Xorg DoCreateContext
            // stores the same fields, glxcmds.c:318-325).
            let Some(req) = x11glx::parse_create_context(minor, body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    crate::nested::GLX_MAJOR_OPCODE,
                );
            };
            let (visual_id, fbconfig) = match req.config {
                x11glx::ContextConfigRef::Visual(visual) => {
                    (visual, glx_visual_fbconfig(visual).unwrap_or(0))
                }
                x11glx::ContextConfigRef::FbConfig(fbconfig) => {
                    (glx_fbconfig_visual(fbconfig), fbconfig)
                }
            };
            state.glx_contexts.insert(
                req.context,
                crate::server::GlxContext {
                    owner: client_id,
                    screen: req.screen,
                    visual_id,
                    fbconfig,
                    render_type: req.render_type,
                    share_list: req.share_list,
                    is_direct: req.is_direct,
                },
            );
            debug!(
                "client {} #{} GLX::CreateContext minor={minor} xid=0x{:x} fbconfig=0x{:x} \
                 visual=0x{visual_id:x} share=0x{:x} direct={}",
                client_id.0, sequence.0, req.context, fbconfig, req.share_list, req.is_direct
            );
        }
        x11glx::DESTROY_CONTEXT => {
            let xid = if body.len() >= 4 {
                u32::from_le_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            state.glx_contexts.remove(&xid);
            debug!(
                "client {} #{} GLX::DestroyContext xid=0x{:x}",
                client_id.0, sequence.0, xid
            );
        }
        x11glx::MAKE_CURRENT | x11glx::MAKE_CONTEXT_CURRENT => {
            // MakeCurrent is a *round-trip* request — Mesa's
            // `glXMakeCurrent` blocks on the contextTag in the reply
            // and labels every subsequent indirect rendering request
            // with it. Direct-rendering clients only use the tag for
            // dispatch identification, but the reply is still
            // mandatory; without it libxcb stalls.
            //
            // The new context XID sits at a minor-specific offset
            // (glxproto.h:225-233, :471-481), body-relative:
            //   minor 5  MakeCurrent:         drawable, context, oldContextTag
            //   minor 26 MakeContextCurrent:  oldContextTag, drawable,
            //                                  readdrawable, context
            let context = if minor == x11glx::MAKE_CURRENT {
                body.get(4..8)
            } else {
                body.get(12..16)
            }
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .unwrap_or(0);
            // The release form (context == None) returns contextTag = 0 —
            // tag 0 is reserved by the protocol to mean "no context
            // current" (server.rs documents this; Xorg vndcmds.c:232-234,
            // :271-273). A malformed/short body also releases.
            let tag = if context == 0 {
                0
            } else {
                let tag = state.glx_next_context_tag;
                state.glx_next_context_tag = state.glx_next_context_tag.wrapping_add(1).max(1);
                tag
            };
            let reply = x11glx::encode_make_current_reply(byte_order, sequence, tag);
            debug!(
                "client {} #{} GLX::MakeCurrent -> contextTag={tag}",
                client_id.0, sequence.0
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::WAIT_GL | x11glx::WAIT_X => {
            // No-op for direct contexts.
            debug!(
                "client {} #{} GLX::Wait{}",
                client_id.0,
                sequence.0,
                if minor == x11glx::WAIT_GL { "GL" } else { "X" }
            );
        }
        x11glx::CREATE_WINDOW | x11glx::CREATE_PIXMAP => {
            let parsed = x11glx::parse_create_glx_window(body);
            if let Some(req) = parsed {
                // For CREATE_PIXMAP: validate the X pixmap exists.
                // CREATE_WINDOW wraps an X window XID, which we don't
                // validate here (windows have a different resource type).
                if minor == x11glx::CREATE_PIXMAP
                    && state
                        .resources
                        .pixmap(yserver_protocol::x11::ResourceId(req.x_window))
                        .is_none()
                {
                    debug!(
                        "client {} #{} GLX::CreatePixmap glx_xid=0x{:x} \
                         x_pixmap=0x{:x} not found -> GLXBadPixmap",
                        client_id.0, sequence.0, req.glx_window, req.x_window
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        crate::nested::GLX_FIRST_ERROR
                            + yserver_protocol::x11::glx::ERROR_GLX_BAD_PIXMAP,
                        req.x_window,
                        u16::from(header.data),
                        crate::nested::GLX_MAJOR_OPCODE,
                    );
                }

                if minor == x11glx::CREATE_PIXMAP {
                    insert_glx_pixmap_record(
                        state,
                        backend,
                        client_id,
                        req.glx_window,
                        req.x_window,
                        req.fbconfig,
                        x11glx::GLX_TEXTURE_2D_EXT,
                    );
                } else {
                    state.glx_drawables.insert(
                        req.glx_window,
                        crate::server::GlxDrawable {
                            owner: client_id,
                            kind: crate::server::GlxDrawableKind::Window,
                            x_drawable: req.x_window,
                            fbconfig: req.fbconfig,
                            width: 0,
                            height: 0,
                            event_mask: 0,
                            glx_export_host_xid: None,
                            texture_target: x11glx::GLX_TEXTURE_2D_EXT,
                        },
                    );
                }

                debug!(
                    "client {} #{} GLX::CreateDrawable minor={minor} \
                     glx_xid=0x{:x} x_drawable=0x{:x} fbconfig=0x{:x}",
                    client_id.0, sequence.0, req.glx_window, req.x_window, req.fbconfig
                );
            } else {
                debug!(
                    "client {} #{} GLX::CreateDrawable minor={minor} (parse failed)",
                    client_id.0, sequence.0
                );
            }
        }
        x11glx::CREATE_PBUFFER => {
            // Pbuffers have NO parent X drawable and carry their size in
            // the request attribs — parse them with the dedicated parser
            // (parse_create_glx_window would mis-read the layout). The
            // stored width/height are reported back as GLX_WIDTH/GLX_HEIGHT
            // from GetDrawableAttributes so Mesa can size the buffer; key
            // the record by the pbuffer XID itself (it is its own drawable).
            if let Some(req) = x11glx::parse_create_pbuffer(body) {
                state.glx_drawables.insert(
                    req.pbuffer,
                    crate::server::GlxDrawable {
                        owner: client_id,
                        kind: crate::server::GlxDrawableKind::Pbuffer,
                        x_drawable: req.pbuffer,
                        fbconfig: req.fbconfig,
                        width: req.width,
                        height: req.height,
                        event_mask: 0,
                        glx_export_host_xid: None,
                        texture_target: yserver_protocol::x11::glx::GLX_TEXTURE_2D_EXT,
                    },
                );
                // #96 tier-2: back the pbuffer with a real GPU pixmap under its
                // own XID, so DRI3 BuffersFromPixmap can export a dmabuf for it.
                // ANGLE (Chromium) renders WebGL into this surface; with no BO
                // it gets BadDrawable and WebGL produces nothing → Google Maps
                // hides the 3D button. Depth comes from the fbconfig's visual.
                let depth = glx_fbconfig_depth(req.fbconfig);
                let pw = u16::try_from(req.width).unwrap_or(1).max(1);
                let ph = u16::try_from(req.height).unwrap_or(1).max(1);
                match backend.create_pixmap(origin, depth, pw, ph) {
                    Ok(handle) => {
                        state.resources.create_pixmap(
                            client_id,
                            x11::CreatePixmapRequest {
                                depth,
                                pixmap: ResourceId(req.pbuffer),
                                drawable: ROOT_WINDOW,
                                width: pw,
                                height: ph,
                            },
                        );
                        let updated = state
                            .resources
                            .set_pixmap_host_xid(ResourceId(req.pbuffer), handle);
                        debug_assert!(updated, "pbuffer backing pixmap was just inserted");
                    }
                    Err(err) => log::warn!(
                        "client {} #{} GLX::CreatePbuffer host pixmap alloc failed: {err}",
                        client_id.0,
                        sequence.0
                    ),
                }
                debug!(
                    "client {} #{} GLX::CreatePbuffer pbuffer=0x{:x} \
                     fbconfig=0x{:x} {}x{}",
                    client_id.0, sequence.0, req.pbuffer, req.fbconfig, req.width, req.height
                );
            } else {
                debug!(
                    "client {} #{} GLX::CreatePbuffer (parse failed)",
                    client_id.0, sequence.0
                );
            }
        }
        x11glx::CREATE_GLX_PIXMAP => {
            // GLX 1.0 glXCreateGLXPixmap: the visual-based counterpart of
            // CreatePixmap. Checks run in Xorg's order — GLXVND's
            // dispatch_CreateGLXPixmap (size, LEGAL_NEW_RESOURCE, screen →
            // BadMatch; glx/vnd_dispatch_stubs.c:143-169), then
            // __glXDisp_CreateGLXPixmap (visual → BadValue) and
            // DoCreateGLXPixmap (dixLookupDrawable → BadDrawable, a window →
            // BadPixmap; glxcmds.c:1198-1220, :1267-1280). Xorg compares
            // neither the pixmap depth nor its visual with the config.
            let glx_error = |state: &mut ServerState, code: u8, value: u32| {
                emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    value,
                    u16::from(minor),
                    crate::nested::GLX_MAJOR_OPCODE,
                )
            };
            let Some(req) = x11glx::parse_create_glx_pixmap(body) else {
                return glx_error(state, x11::error::BAD_LENGTH, 0);
            };
            let owned = state.clients.get(&client_id.0).is_some_and(|c| {
                crate::server::IdAllocator::validate_owned(
                    req.glx_pixmap,
                    c.resource_id_base,
                    c.resource_id_mask,
                )
            });
            if !owned || state.xid_occupied(req.glx_pixmap) {
                return glx_error(state, x11::error::BAD_ID_CHOICE, req.glx_pixmap);
            }
            // yserver has exactly one screen (screenInfo.numScreens == 1).
            if req.screen != 0 {
                return glx_error(state, x11::error::BAD_MATCH, req.screen);
            }
            let Some(fbconfig) = glx_visual_fbconfig(req.visual) else {
                return glx_error(state, x11::error::BAD_VALUE, req.visual);
            };
            if state.resources.pixmap(ResourceId(req.pixmap)).is_none() {
                let code = if state.resources.window(ResourceId(req.pixmap)).is_some() {
                    x11::error::BAD_PIXMAP
                } else {
                    x11::error::BAD_DRAWABLE
                };
                return glx_error(state, code, req.pixmap);
            }
            // Xorg never runs determineTextureTarget for the GLX 1.0 form,
            // so the target stays 0 and GetDrawableAttributes reports
            // GLX_TEXTURE_RECTANGLE_EXT (glxcmds.c:1896-1897).
            insert_glx_pixmap_record(
                state,
                backend,
                client_id,
                req.glx_pixmap,
                req.pixmap,
                fbconfig,
                x11glx::GLX_TEXTURE_RECTANGLE_EXT,
            );
            debug!(
                "client {} #{} GLX::CreateGLXPixmap glx_xid=0x{:x} x_pixmap=0x{:x} \
                 visual=0x{:x} fbconfig=0x{fbconfig:x}",
                client_id.0, sequence.0, req.glx_pixmap, req.pixmap, req.visual
            );
        }
        x11glx::DESTROY_GLX_PIXMAP => {
            // GLX 1.0 glXDestroyGLXPixmap. Only a live GLX pixmap — created
            // by either CreateGLXPixmap or CreatePixmap, which Xorg keeps as
            // the same GLX_DRAWABLE_PIXMAP type — may be destroyed; any other
            // XID is GLXBadPixmap (glx/vnd_dispatch_stubs.c:189-205,
            // glxcmds.c validGlxDrawable / DoDestroyDrawable).
            let Some(xid) = x11glx::parse_single_xid(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    crate::nested::GLX_MAJOR_OPCODE,
                );
            };
            let is_glx_pixmap = state
                .glx_drawables
                .get(&xid)
                .is_some_and(|d| d.kind == crate::server::GlxDrawableKind::Pixmap);
            if !is_glx_pixmap {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_PIXMAP,
                    xid,
                    u16::from(minor),
                    crate::nested::GLX_MAJOR_OPCODE,
                );
            }
            // Release the export ref through the host xid stored at create
            // time, never by re-resolving the X pixmap: it may already be
            // freed (see the DESTROY_PIXMAP arm).
            if let Some(record) = state.glx_drawables.remove(&xid)
                && let Some(host_xid) = record.glx_export_host_xid
            {
                backend.release_glx_pixmap_export(host_xid);
            }
            debug!(
                "client {} #{} GLX::DestroyGLXPixmap glx_xid=0x{xid:x}",
                client_id.0, sequence.0
            );
        }
        x11glx::DELETE_WINDOW | x11glx::DESTROY_PIXMAP | x11glx::DESTROY_PBUFFER => {
            let xid = if body.len() >= 4 {
                u32::from_le_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            // For DESTROY_PIXMAP: release the export-lifetime ref taken at
            // CREATE_PIXMAP, using the host_xid resolved+stored at acquire
            // time. Do NOT re-resolve via resources.pixmap(x_drawable) — the
            // X pixmap may already be freed (FreePixmap-before-destroy), and
            // re-resolution would return None and leak the ref forever.
            let release_host_xid = if minor == x11glx::DESTROY_PIXMAP {
                state
                    .glx_drawables
                    .get(&xid)
                    .and_then(|d| d.glx_export_host_xid)
            } else {
                None
            };
            if let Some(host_xid) = release_host_xid {
                backend.release_glx_pixmap_export(host_xid);
            }
            state.glx_drawables.remove(&xid);
            // #96 tier-2: release the GPU pixmap that backed a pbuffer (allocated
            // at CreatePbuffer under the pbuffer's own XID).
            if minor == x11glx::DESTROY_PBUFFER
                && let Some(pixmap) = state.resources.free_pixmap(ResourceId(xid))
                && let Some(host) = pixmap.host_xid
            {
                backend.free_pixmap(origin, host.as_raw())?;
            }
            debug!(
                "client {} #{} GLX::DestroyDrawable minor={minor} glx_xid=0x{:x}",
                client_id.0, sequence.0, xid
            );
        }
        x11glx::CHANGE_DRAWABLE_ATTRIBUTES => {
            // body: [glx_drawable: u32][num_attribs: u32][attribs * (id, value)]
            if body.len() >= 8 {
                let xid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let num_attribs = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
                if let Some(drawable) = state.glx_drawables.get_mut(&xid) {
                    // Xorg's ChangeDrawableAttributes is a switch with a
                    // single case: record GLX_EVENT_MASK, silently ignore
                    // everything else (glxcmds.c:1494-1503).
                    let mut p = 8;
                    for _ in 0..num_attribs {
                        if p + 8 > body.len() {
                            break;
                        }
                        let id =
                            u32::from_le_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
                        let val = u32::from_le_bytes([
                            body[p + 4],
                            body[p + 5],
                            body[p + 6],
                            body[p + 7],
                        ]);
                        if id == x11glx::GLX_EVENT_MASK {
                            drawable.event_mask = val;
                        }
                        p += 8;
                    }
                }
                debug!(
                    "client {} #{} GLX::ChangeDrawableAttributes glx_xid=0x{:x} n={num_attribs}",
                    client_id.0, sequence.0, xid
                );
            }
        }
        x11glx::GET_DRAWABLE_ATTRIBUTES => {
            // body: [glx_drawable: u32]. Mirrors Xorg's four-way
            // behaviour, where GLXVND is the front door: a registered
            // GLX drawable gets the full reply; a naked X window (the
            // GLX 1.2 pattern — Mesa's `pixmap_from_buffer` +
            // `glXMakeCurrent(window, ctx)` queries the X window XID
            // directly) gets the reply without the pGlxDraw block; a
            // naked X pixmap is forwarded by GLXVND (pixmaps carry
            // RC_DRAWABLE) but fails dixLookupWindow → GLXBadDrawable;
            // an XID that is not a drawable at all never reaches
            // DoGetDrawableAttributes → core BadDrawable
            // (glx/vnd_dispatch_stubs.c:456-472, glxcmds.c:1873-1880).
            let xid = if body.len() >= 4 {
                u32::from_le_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            if !state.glx_drawables.contains_key(&xid)
                && state.resources.window(ResourceId(xid)).is_none()
            {
                if state.resources.pixmap(ResourceId(xid)).is_some() {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_DRAWABLE,
                        xid,
                        u16::from(header.data),
                        crate::nested::GLX_MAJOR_OPCODE,
                    );
                }
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    xid,
                    u16::from(header.data),
                    crate::nested::GLX_MAJOR_OPCODE,
                );
            }
            let attribs = drawable_attributes_for(state, xid);
            let reply =
                x11glx::encode_get_drawable_attributes_reply(byte_order, sequence, &attribs);
            debug!(
                "client {} #{} GLX::GetDrawableAttributes glx_xid=0x{:x} -> {} attribs",
                client_id.0,
                sequence.0,
                xid,
                attribs.len()
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::SWAP_BUFFERS => {
            // Direct-rendering clients route swaps through DRI3 +
            // Present, never through indirect GLX SwapBuffers. Indirect
            // clients hitting this path can't be served (no server-side
            // GL), but a no-op (rather than an error) keeps silly
            // clients limping along.
            debug!(
                "client {} #{} GLX::SwapBuffers (no-op)",
                client_id.0, sequence.0
            );
        }
        x11glx::QUERY_CONTEXT => {
            // GLX 1.3 QueryContext, the request behind
            // glXImportContextEXT (GLX_EXT_import_context, which, like Xorg
            // without +iglx, we do not advertise). Xorg's DoQueryContext reports five
            // attributes, in this order, for a live context of ANY client
            // (glxcmds.c:1659-1708); an XID that is not a context is
            // GLXBadContext (glx/vnd_dispatch_stubs.c:492-508).
            let context = match glx_request_context(state, body) {
                Ok(context) => context,
                Err((code, value)) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        code,
                        value,
                        u16::from(minor),
                        crate::nested::GLX_MAJOR_OPCODE,
                    );
                }
            };
            let ctx = &state.glx_contexts[&context];
            let attribs = [
                (x11glx::GLX_SHARE_CONTEXT_EXT, ctx.share_list),
                (x11glx::GLX_VISUAL_ID, ctx.visual_id),
                (x11glx::GLX_SCREEN, ctx.screen),
                (x11glx::GLX_FBCONFIG_ID, ctx.fbconfig),
                (x11glx::GLX_RENDER_TYPE, ctx.render_type),
            ];
            let reply = x11glx::encode_query_context_reply(byte_order, sequence, &attribs);
            debug!(
                "client {} #{} GLX::QueryContext 0x{context:x} -> {attribs:x?}",
                client_id.0, sequence.0
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11glx::VENDOR_PRIVATE | x11glx::VENDOR_PRIVATE_WITH_REPLY => {
            // Dispatch known vendor codes; reject everything else with
            // GLXUnsupportedPrivateRequest (modern Mesa direct-rendering
            // never reaches this path).
            //
            // VendorPrivate body layout:
            //   [0..4]   vendor_code  (u32 LE)
            //   [4..8]   context_tag  (u32 LE, informational)
            //   [8..12]  glx_drawable (u32 LE, GLXPixmap XID)
            //   [12..16] buffer       (u32 LE, e.g. GLX_FRONT_LEFT_EXT)
            let vendor_code = if body.len() >= 4 {
                u32::from_le_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            match vendor_code {
                x11glx::VENDOR_CODE_BIND_TEX_IMAGE => {
                    // glXBindTexImageEXT (indirect path, EXT completeness).
                    // Direct-context compositors (muffin) ride DRI3 and never
                    // hit this. For indirect contexts: resolve the GLXPixmap to
                    // its underlying X pixmap, ensure it is export-promoted
                    // (so any indirect GL texture sampling reads live content),
                    // and return success. BindTexImageEXT has no reply.
                    let glx_drawable = if body.len() >= 12 {
                        u32::from_le_bytes([body[8], body[9], body[10], body[11]])
                    } else {
                        0
                    };
                    debug!(
                        "client {} #{} GLX::BindTexImageEXT glx_drawable=0x{glx_drawable:x}",
                        client_id.0, sequence.0
                    );
                    if let Some(d) = state.glx_drawables.get(&glx_drawable) {
                        let x_drawable = d.x_drawable;
                        let stored_host_xid = d.glx_export_host_xid;
                        let current_host_xid = state
                            .resources
                            .pixmap(yserver_protocol::x11::ResourceId(x_drawable))
                            .and_then(|p| p.host_xid.map(|h| h.as_raw()));
                        let host_xid_for_export = current_host_xid.or(stored_host_xid);
                        if let Some(current_host_xid) = current_host_xid
                            && stored_host_xid != Some(current_host_xid)
                        {
                            retarget_glx_pixmap_export(
                                state,
                                backend,
                                glx_drawable,
                                current_host_xid,
                            );
                        }
                        // Ensure the backing is promoted to exportable storage so
                        // indirect GL texture sampling can read live content.
                        //
                        // CRITICAL: use promote_pixmap_exportable, NOT
                        // acquire_glx_pixmap_export. Bind is decoupled from the
                        // GLXPixmap LIFETIME refcount (glx_refs): TFP compositors
                        // rebind every frame / on damage (the spec allows rebind
                        // without an intervening release), so calling acquire here
                        // would grow glx_refs unboundedly and the backing would
                        // never tear down after glXDestroyPixmap. promote is
                        // idempotent (the engine short-circuits via is_exportable)
                        // and touches no refcount. The backing is already kept
                        // alive by glXCreatePixmap's acquire (glx_refs >= 1 until
                        // glXDestroyPixmap).
                        //
                        // TODO(glx-tfp): indirect texture *sampling* (serving
                        // glTexImage2D-equivalent data from the promoted backing)
                        // is not yet implemented; the bind succeeds so clients
                        // do not receive a protocol error, but the texture
                        // content will not be updated until indirect GL sampling
                        // is wired up.
                        if let Some(host_xid) = host_xid_for_export {
                            let _ = backend.promote_pixmap_exportable(host_xid);
                        }
                        // No reply for VENDOR_PRIVATE; for VENDOR_PRIVATE_WITH_REPLY
                        // send an empty success reply so the client does not block.
                        if minor == x11glx::VENDOR_PRIVATE_WITH_REPLY {
                            let reply = x11glx::encode_get_drawable_attributes_reply(
                                byte_order,
                                sequence,
                                &[],
                            );
                            let Some(client) = state.clients.get_mut(&client_id.0) else {
                                return Ok(RequestOutcome::Handled);
                            };
                            return Ok(write_to_client(client, client_id, &reply));
                        }
                    } else {
                        debug!(
                            "client {} #{} GLX::BindTexImageEXT \
                             glx_drawable=0x{glx_drawable:x} not found -> GLXBadPixmap",
                            client_id.0, sequence.0
                        );
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            crate::nested::GLX_FIRST_ERROR
                                + yserver_protocol::x11::glx::ERROR_GLX_BAD_PIXMAP,
                            glx_drawable,
                            u16::from(header.data),
                            crate::nested::GLX_MAJOR_OPCODE,
                        );
                    }
                }
                x11glx::VENDOR_CODE_GET_FB_CONFIGS_SGIX => {
                    // glXGetFBConfigsSGIX (VendorPrivateWithReply, vendor
                    // code 65540).  Proxied to synthesise_glx_fb_configs
                    // and encoded with the GLX 1.3 GetFBConfigsReply wire
                    // format — glxproto.h defines no separate SGIX reply
                    // struct; the extension reuses the same layout.
                    //
                    // Request body:
                    //   [0..4]  vendorCode (already read)
                    //   [4..8]  pad1
                    //   [8..12] screen — intentionally ignored (yserver is
                    //           single-screen; configs are valid for any screen,
                    //           same as the GLX 1.3 GET_FB_CONFIGS handler).
                    // GetFBConfigsSGIX is a reply-bearing call; if a malformed
                    // client sends it as plain VENDOR_PRIVATE (no reply expected)
                    // do NOT write a reply, or we desync its reply stream.
                    if minor != x11glx::VENDOR_PRIVATE_WITH_REPLY {
                        return Ok(RequestOutcome::Handled);
                    }
                    let configs = synthesise_glx_fb_configs(state.glx_tfp_supported);
                    let config_refs: Vec<&[(u32, u32)]> =
                        configs.iter().map(|c| c.as_slice()).collect();
                    let reply = x11glx::encode_get_fb_configs_sgix_reply(
                        byte_order,
                        sequence,
                        &config_refs,
                    );
                    debug!(
                        "client {} #{} GLX::GetFBConfigsSGIX -> {} configs × {} props",
                        client_id.0,
                        sequence.0,
                        config_refs.len(),
                        config_refs.first().map_or(0, |c| c.len()),
                    );
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        return Ok(RequestOutcome::Handled);
                    };
                    return Ok(write_to_client(client, client_id, &reply));
                }
                x11glx::VENDOR_CODE_CREATE_CONTEXT_WITH_CONFIG_SGIX => {
                    // glXCreateContextWithConfigSGIX (VendorPrivate, no
                    // reply, vendor code 65541).  Proxied to the existing
                    // context-creation logic: insert a GlxContext record.
                    //
                    // Request body:
                    //   [0..4]  vendorCode
                    //   [4..8]  pad1
                    //   [8..12] context XID
                    //   [12..16] fbconfig
                    //   [16..20] screen
                    //   [20..24] renderType
                    //   [24..28] shareList
                    //   [28]    isDirect
                    //   [29..32] reserved
                    if let Some(req) = x11glx::parse_create_context_with_config_sgix(body) {
                        state.glx_contexts.insert(
                            req.context,
                            crate::server::GlxContext {
                                owner: client_id,
                                screen: req.screen,
                                visual_id: glx_fbconfig_visual(req.fbconfig),
                                fbconfig: req.fbconfig,
                                render_type: req.render_type,
                                share_list: req.share_list,
                                is_direct: req.is_direct,
                            },
                        );
                        debug!(
                            "client {} #{} GLX::CreateContextWithConfigSGIX \
                             xid=0x{:x} fbconfig=0x{:x}",
                            client_id.0, sequence.0, req.context, req.fbconfig
                        );
                    } else {
                        debug!(
                            "client {} #{} GLX::CreateContextWithConfigSGIX \
                             (body too short, parse failed)",
                            client_id.0, sequence.0
                        );
                    }
                }
                x11glx::VENDOR_CODE_CREATE_GLX_PIXMAP_WITH_CONFIG_SGIX => {
                    // glXCreateGLXPixmapWithConfigSGIX (VendorPrivate, no
                    // reply, vendor code 65542).  Proxied to the existing
                    // CREATE_PIXMAP path: validate X pixmap, record the
                    // GlxDrawable, call acquire_glx_pixmap_export.
                    //
                    // Request body:
                    //   [0..4]  vendorCode
                    //   [4..8]  pad1
                    //   [8..12]  screen
                    //   [12..16] fbconfig
                    //   [16..20] pixmap (X pixmap XID)
                    //   [20..24] glxpixmap (GLXPixmap XID)
                    if let Some(req) = x11glx::parse_create_glx_pixmap_with_config_sgix(body) {
                        // Validate that the X pixmap exists.
                        if state
                            .resources
                            .pixmap(yserver_protocol::x11::ResourceId(req.pixmap))
                            .is_none()
                        {
                            debug!(
                                "client {} #{} GLX::CreateGLXPixmapWithConfigSGIX \
                                 glx_xid=0x{:x} x_pixmap=0x{:x} not found -> GLXBadPixmap",
                                client_id.0, sequence.0, req.glx_pixmap, req.pixmap
                            );
                            return emit_x11_error_with_minor(
                                state,
                                client_id,
                                sequence,
                                crate::nested::GLX_FIRST_ERROR
                                    + yserver_protocol::x11::glx::ERROR_GLX_BAD_PIXMAP,
                                req.pixmap,
                                u16::from(header.data),
                                crate::nested::GLX_MAJOR_OPCODE,
                            );
                        }
                        insert_glx_pixmap_record(
                            state,
                            backend,
                            client_id,
                            req.glx_pixmap,
                            req.pixmap,
                            req.fbconfig,
                            x11glx::GLX_TEXTURE_2D_EXT,
                        );

                        debug!(
                            "client {} #{} GLX::CreateGLXPixmapWithConfigSGIX \
                             glx_xid=0x{:x} x_pixmap=0x{:x} fbconfig=0x{:x}",
                            client_id.0, sequence.0, req.glx_pixmap, req.pixmap, req.fbconfig
                        );
                    } else {
                        debug!(
                            "client {} #{} GLX::CreateGLXPixmapWithConfigSGIX \
                             (body too short, parse failed)",
                            client_id.0, sequence.0
                        );
                    }
                }
                x11glx::VENDOR_CODE_RELEASE_TEX_IMAGE => {
                    // glXReleaseTexImageEXT (indirect path, EXT completeness).
                    //
                    // CRITICAL: this is a LIFETIME no-op. It must NOT call
                    // release_glx_pixmap_export — that decrements the
                    // GLXPixmap's create-time lifetime ref (glx_refs), which
                    // glXDestroyPixmap owns. A release without a matching bind
                    // (or more releases than binds) would drive glx_refs to 0
                    // and tear down the backing WHILE THE GLXPixmap IS STILL
                    // ALIVE. Release only ends the texture binding; since bind
                    // is promote-only (no refcount taken), there is nothing to
                    // undo here. The promoted storage stays promoted (cheap,
                    // and a likely-imminent rebind reuses it). Unknown GLXPixmap
                    // XIDs are silently ignored (no error — matches Xorg
                    // glx/glxcmds.c behaviour for stale release after destroy).
                    let glx_drawable = if body.len() >= 12 {
                        u32::from_le_bytes([body[8], body[9], body[10], body[11]])
                    } else {
                        0
                    };
                    debug!(
                        "client {} #{} GLX::ReleaseTexImageEXT glx_drawable=0x{glx_drawable:x} \
                         (lifetime no-op)",
                        client_id.0, sequence.0
                    );
                    // No reply for VENDOR_PRIVATE; for VENDOR_PRIVATE_WITH_REPLY
                    // send an empty success reply.
                    if minor == x11glx::VENDOR_PRIVATE_WITH_REPLY {
                        let reply =
                            x11glx::encode_get_drawable_attributes_reply(byte_order, sequence, &[]);
                        let Some(client) = state.clients.get_mut(&client_id.0) else {
                            return Ok(RequestOutcome::Handled);
                        };
                        return Ok(write_to_client(client, client_id, &reply));
                    }
                }
                _ => {
                    // All other vendor codes are unsupported.
                    debug!(
                        "client {} #{} GLX::VendorPrivate \
                         vendor_code={vendor_code} (rejected: GLXUnsupportedPrivateRequest)",
                        client_id.0, sequence.0
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        crate::nested::GLX_FIRST_ERROR
                            + yserver_protocol::x11::glx::ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST,
                        0,
                        u16::from(header.data),
                        crate::nested::GLX_MAJOR_OPCODE,
                    );
                }
            }
        }
        other => {
            // Indirect-rendering opcodes 1..=198 + anything else we
            // don't handle → GLXBadRequest.
            debug!(
                "client {} #{} GLX unsupported minor={other} -> GLXBadRequest",
                client_id.0, sequence.0
            );
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                crate::nested::GLX_FIRST_ERROR
                    + yserver_protocol::x11::glx::ERROR_GLX_BAD_RENDER_REQUEST,
                0,
                u16::from(header.data),
                crate::nested::GLX_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// Record a GLXPixmap over the (already validated) X pixmap `x_pixmap` and
/// take its export-lifetime ref — the one resource model behind GLX 1.0
/// `CreateGLXPixmap`, GLX 1.3 `CreatePixmap` and
/// `CreateGLXPixmapWithConfigSGIX`, as Xorg funnels all three through
/// `DoCreateGLXPixmap` (glxcmds.c:1198-1220).
///
/// The host xid is resolved NOW and stored on the record so release is
/// robust to the X pixmap being freed (X11 `FreePixmap`) before the GLX
/// destroy / disconnect — re-resolving `x_drawable → host_xid` at release
/// time would fail once the resource is gone, leaking the ref forever. The
/// ref keeps the `ExportedBacking` entry (and its dmabuf fd) alive past an
/// early `FreePixmap`, the counterpart of Xorg's `pixmap->refcnt++`.
fn insert_glx_pixmap_record(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    owner: ClientId,
    glx_pixmap: u32,
    x_pixmap: u32,
    fbconfig: u32,
    texture_target: u32,
) {
    let acquire_host_xid = state
        .resources
        .pixmap(ResourceId(x_pixmap))
        .and_then(|p| p.host_xid.map(|h| h.as_raw()));
    state.glx_drawables.insert(
        glx_pixmap,
        crate::server::GlxDrawable {
            owner,
            kind: crate::server::GlxDrawableKind::Pixmap,
            x_drawable: x_pixmap,
            fbconfig,
            width: 0,
            height: 0,
            event_mask: 0,
            glx_export_host_xid: acquire_host_xid,
            texture_target,
        },
    );
    if let Some(host_xid) = acquire_host_xid {
        backend.acquire_glx_pixmap_export(host_xid);
    }
}

/// The context XID of a `QueryContext` / `IsDirect` request, or the error
/// Xorg sends: `BadLength` for a body that is not exactly one XID
/// (`REQUEST_SIZE_MATCH`), `GLXBadContext` with the XID as bad value when
/// it names no live context (glx/vnd_dispatch_stubs.c:492-525).
fn glx_request_context(state: &ServerState, body: &[u8]) -> Result<u32, (u8, u32)> {
    use yserver_protocol::x11::glx as g;
    let Some(xid) = g::parse_single_xid(body) else {
        return Err((x11::error::BAD_LENGTH, 0));
    };
    if state.glx_contexts.contains_key(&xid) {
        Ok(xid)
    } else {
        Err((
            crate::nested::GLX_FIRST_ERROR + g::ERROR_GLX_BAD_CONTEXT,
            xid,
        ))
    }
}

/// The FBConfig behind a GLX *visual*, or `None` when `visual` is not one
/// of the GLX visuals `GetVisualConfigs` advertises. Mirrors Xorg's
/// `validGlxVisual`, which searches `pGlxScreen->visuals` only — an
/// FBConfig ID, or an X visual without a GLX config, is not a GLX visual
/// (glxcmds.c:90-107). `synthesise_glx_fb_configs` stays the single source
/// of the visual → FBConfig pairing.
pub(super) fn glx_visual_fbconfig(visual: u32) -> Option<u32> {
    use yserver_protocol::x11::glx as g;
    if !synthesise_glx_visual_configs()
        .iter()
        .any(|v| v.visual_id == visual)
    {
        return None;
    }
    synthesise_glx_fb_configs(false).iter().find_map(|cfg| {
        cfg.iter()
            .any(|(a, v)| *a == g::GLX_VISUAL_ID && *v == visual)
            .then(|| {
                cfg.iter()
                    .find(|(a, _)| *a == g::GLX_FBCONFIG_ID)
                    .map(|(_, v)| *v)
            })
            .flatten()
    })
}

/// The X visual of a synthesised FBConfig (`GLX_VISUAL_ID`), 0 for a
/// visual-less or unknown FBConfig — Xorg's `ctx->config->visualID`.
pub(super) fn glx_fbconfig_visual(fbconfig: u32) -> u32 {
    use yserver_protocol::x11::glx as g;
    synthesise_glx_fb_configs(false)
        .iter()
        .find(|cfg| {
            cfg.iter()
                .any(|(a, v)| *a == g::GLX_FBCONFIG_ID && *v == fbconfig)
        })
        .and_then(|cfg| {
            cfg.iter()
                .find(|(a, _)| *a == g::GLX_VISUAL_ID)
                .map(|(_, v)| *v)
        })
        .unwrap_or(0)
}

/// Depth (24 or 32) of a synthesised GLX FBConfig, derived from the X visual
/// it maps to (`ROOT_VISUAL` depth-24 / `ARGB_VISUAL` depth-32) — the single
/// source of truth is `synthesise_glx_fb_configs`, so this stays correct if
/// the config table changes. Defaults to 24 for an unknown fbconfig.
pub(super) fn glx_fbconfig_depth(fbconfig: u32) -> u8 {
    use yserver_protocol::x11::glx as g;
    synthesise_glx_fb_configs(false)
        .iter()
        .find(|cfg| {
            cfg.iter()
                .any(|(a, v)| *a == g::GLX_FBCONFIG_ID && *v == fbconfig)
        })
        .and_then(|cfg| {
            cfg.iter()
                .find(|(a, _)| *a == g::GLX_VISUAL_ID)
                .map(|(_, v)| *v)
        })
        .map_or(24, |visual| {
            if visual == crate::resources::ARGB_VISUAL.0 {
                32
            } else {
                24
            }
        })
}
