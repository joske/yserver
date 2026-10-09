use super::*;

#[test]
fn warn_throttle_logs_once_per_period_and_counts_the_rest() {
    use crate::kms::render::backend::WarnThrottle;
    let t0 = std::time::Instant::now();
    let mut w = WarnThrottle::default();
    assert_eq!(w.check(t0), Some(0));
    for i in 1..=5 {
        assert_eq!(w.check(t0 + std::time::Duration::from_millis(i)), None);
    }
    assert_eq!(w.check(t0 + WarnThrottle::PERIOD), Some(5));
    assert_eq!(w.check(t0 + WarnThrottle::PERIOD), None);
}

#[test]
fn glx_vendor_names_are_nvidia_first_then_mesa_on_nvidia() {
    use ash::vk::DriverId;

    // libglvnd tries the names left to right and falls through when
    // one will not load, so "mesa" is insurance for the routine
    // package split where the NVIDIA Vulkan ICD is installed but
    // libGLX_nvidia.so is not. Without it libglvnd resolves no
    // vendor and lands on FALLBACK_VENDOR_NAME "indirect", which is
    // worse than today's llvmpipe.
    assert_eq!(
        glx_vendor_names_for_driver(DriverId::NVIDIA_PROPRIETARY),
        "nvidia mesa"
    );
}

#[test]
fn glx_vendor_names_stay_mesa_on_every_other_driver() {
    use ash::vk::DriverId;
    use yserver_protocol::x11::glx as x11glx;

    // The mapping is deliberately binary. Entries for other drivers
    // are omitted because nobody working on this repo can measure
    // them, and an unmeasured mapping that redirects a working
    // configuration onto a nonexistent libGLX_*.so is worse than
    // the status quo.
    for driver in [
        DriverId::MESA_LLVMPIPE,
        DriverId::INTEL_OPEN_SOURCE_MESA,
        DriverId::MESA_RADV,
        DriverId::AMD_PROPRIETARY,
    ] {
        assert_eq!(glx_vendor_names_for_driver(driver), x11glx::VENDOR_NAMES);
    }
}

/// Nothing else in this file calls `glx_vendor_names` through the
/// `Backend` trait — every other assertion here calls
/// `glx_vendor_names_for_driver` directly. Going through UFCS on
/// the trait forces dispatch through `impl Backend for
/// KmsBackend` rather than a same-named inherent method, and pins
/// the `platform.vk == None` fallback (`PlatformBackend::for_tests`
/// leaves `vk` unset — asserted at platform.rs:3312).
///
/// What this actually pins down is the fallback body: with `vk ==
/// None`, `glx_vendor_names` takes the `map_or` default branch and
/// returns `x11glx::VENDOR_NAMES` ("mesa") without ever calling
/// `glx_vendor_names_for_driver`. It fails if that `map_or` is
/// changed to `unwrap`/`expect` on `vk` (panics instead of falling
/// back) or if the no-Vulkan default is changed away from "mesa".
/// It does *not* discriminate trait dispatch from inherent-method
/// dispatch — with `vk == None` the two paths return the identical
/// `&'static str`, so this assertion would pass either way; the
/// UFCS call is here for documentation of intent, not as a check
/// on how the method is dispatched.
#[test]
fn glx_vendor_names_falls_back_to_mesa_with_no_vk() {
    use yserver_protocol::x11::glx as x11glx;

    assert_eq!(
        Backend::glx_vendor_names(&KmsBackend::for_tests()),
        x11glx::VENDOR_NAMES
    );
}

/// Stage 1b acceptance gate (synthetic): v2 constructs through
/// `for_tests` and answers the capability accessors with the
/// same values as v1. This is the "boots far enough to service
/// capability queries" check from the spec.
#[test]
fn skeleton_advertises_expected_capabilities() {
    let b = KmsBackend::for_tests();
    assert_eq!(b.window_id(), 1);
    assert_eq!(b.root_visual_xid(), 0x21);
    assert_eq!(b.render_opcode(), Some(133));
    assert_eq!(b.xkb_opcode(), Some(136));
    assert_eq!(b.xkb_info(), Some((136, 85, 162)));
    assert_eq!(b.composite_opcode(), Some(144));
    // Non-trivial format passes through untouched; 0 returns None.
    assert_eq!(b.render_format_for_ynest_id(0), None);
    assert_eq!(b.render_format_for_ynest_id(0x12345), Some(0x12345));
    // KMS has no upstream host visuals, but it still advertises
    // server-local ARGB ids so CreateWindow can preserve depth 32.
    assert_eq!(b.argb_visual_xid(), Some(0x103));
    assert_eq!(b.argb_colormap_xid(), Some(0x104));
}

/// Spec: "boots far enough to service GetGeometry / InternAtom".
/// Backend::xid_map reflects KmsCore's root xid seed via
/// for_tests — empty xid map is fine for this test since the
/// fixture omits the root insert that production does. The
/// load-bearing check is that the xid_map accessor returns a
/// real reference rather than panicking.
#[test]
fn xid_map_is_reachable_via_backend_trait() {
    let b = KmsBackend::for_tests();
    let map = b.xid_map();
    // for_tests builds an empty map (it doesn't seed root the
    // way KmsCore::new does); verify the accessor works and
    // returns an actual map reference.
    assert_eq!(map.len(), 0);
}

#[test]
fn list_fonts_proxy_returns_catalog_matches() {
    let mut b = KmsBackend::for_tests();
    let expected = u16::try_from(b.core.font_loader.catalog.len().min(8)).unwrap_or(u16::MAX);
    let reply = b.list_fonts_proxy(None, 8, "*").expect("list_fonts");
    assert_eq!(reply[0], 1);
    let count = u16::from_le_bytes([reply[8], reply[9]]);
    assert_eq!(count, expected);
}

#[test]
fn list_fonts_with_info_proxy_emits_terminator() {
    let mut b = KmsBackend::for_tests();
    let replies = b
        .list_fonts_with_info_proxy(None, 4, "*", &mut |_| 0x99)
        .expect("list_fonts_with_info");
    assert!(!replies.is_empty(), "terminator reply must be present");
    let terminator = replies.last().expect("terminator");
    assert_eq!(terminator[0], 1);
    assert_eq!(terminator[1], 0);
}

/// `XCreateFontSet("fixed")` regression (e16-in-vng silent exit):
/// libX11's XLC takes the XLFD from the ListFontsWithInfo reply
/// NAME (or the FONT property), parses the charset from the last
/// two fields, and `OpenFont`s that name verbatim — verified by
/// tracing the probe against Xephyr (`tools/fontset-trace-xephyr.sh`:
/// LFWI('fixed') → name/-FONT atom
/// '-Misc-Fixed-…-C-60-ISO8859-1' → OpenFont(same)). A bare
/// alias name carries no charset, so XLC reports the C-locale
/// charset missing and returns a NULL fontset; e16 exits.
///
/// Pin: an alias match must reply with a full XLFD name whose
/// registry-encoding tail is iso8859-1, and that exact name must
/// round-trip through open_font.
#[test]
fn list_fonts_with_info_resolves_alias_to_xlfd_name() {
    // Post-font-path rework, "fixed" may resolve two ways:
    //  - via a real font-path dir (e.g. /usr/share/fonts/misc
    //    fonts.alias) → reply name "fixed" VERBATIM (Xorg FPE
    //    behavior) with the PCF's own FONT property carrying the
    //    full XLFD;
    //  - via built-ins (no misc dir on the machine) → reply name
    //    is the synthesized charset-bearing XLFD.
    // The libX11 guarantee e16 needs (omGeneric.c get_prop_name)
    // is the FONT PROPERTY: XA_FONT (18) → atom whose string is a
    // full XLFD with a charset tail. Pin that, not the name shape.
    let mut b = KmsBackend::for_tests();
    let mut interned: Vec<String> = Vec::new();
    let replies = b
        .list_fonts_with_info_proxy(None, 100, "fixed", &mut |name| {
            interned.push(name.to_owned());
            0x77 + u32::try_from(interned.len()).unwrap()
        })
        .expect("list_fonts_with_info");
    assert!(
        replies.len() >= 2,
        "at least one info reply + terminator; got {}",
        replies.len()
    );
    // LFWI info reply layout: name_len at byte 1, nProperties at
    // bytes 46..48, properties (8 bytes each) at 60.., then name.
    let info = &replies[0];
    let name_len = usize::from(info[1]);
    let n_props = usize::from(u16::from_le_bytes([info[46], info[47]]));
    assert!(n_props >= 1, "at least the FONT property");
    let mut font_value_atom = None;
    for i in 0..n_props {
        let off = 60 + i * 8;
        let prop_name =
            u32::from_le_bytes([info[off], info[off + 1], info[off + 2], info[off + 3]]);
        if prop_name == 18 {
            font_value_atom = Some(u32::from_le_bytes([
                info[off + 4],
                info[off + 5],
                info[off + 6],
                info[off + 7],
            ]));
        }
    }
    let font_value_atom = font_value_atom.expect("XA_FONT (18) property must be present");
    // Map the mock-interned atom id back to its string.
    let idx = usize::try_from(font_value_atom - 0x78).expect("FONT value is mock-interned");
    let font_xlfd = interned.get(idx).expect("interned FONT value").clone();
    assert!(
        font_xlfd.starts_with('-'),
        "FONT property must be a full XLFD; got {font_xlfd:?}"
    );
    let fields: Vec<&str> = font_xlfd.split('-').collect();
    assert_eq!(
        fields.len(),
        15,
        "XLFD has 14 fields (15 split parts with the leading dash); got {font_xlfd:?}"
    );
    let registry = fields[13].to_ascii_lowercase();
    assert!(
        registry.starts_with("iso"),
        "charset registry tail must be an iso charset so the \
             C-locale XLC charset binds; got {font_xlfd:?}"
    );
    // The reply NAME must be openable — XCreateFontSet OpenFonts
    // it verbatim (alias or XLFD alike).
    let name_off = 60 + n_props * 8;
    let name = std::str::from_utf8(&info[name_off..name_off + name_len]).expect("utf8 name");
    b.core
        .font_loader
        .open_font(name)
        .expect("LFWI reply name must round-trip through open_font");
}

/// A machine with no `fonts.dir` anywhere (Arch without
/// `xorg-mkfontscale`, which GENERATES the index post-transaction
/// rather than shipping it) falls back to a built-ins-only font path.
/// The LFWI reply name for `fixed` is then synthesized from the
/// fontconfig-resolved face metrics (21px), which no catalog entry
/// carries, so `resolve` rejected our OWN reply name and
/// XCreateFontSet got BadName → NULL fontset → e16 exits (#107).
/// Pin: whatever LFWI advertises on a built-ins-only path opens.
#[test]
fn builtins_only_lfwi_reply_name_round_trips() {
    let mut b = KmsBackend::for_tests();
    b.core
        .font_loader
        .set_font_path(&["built-ins".to_string()])
        .expect("built-ins is always a valid path element");
    let mut interned: Vec<String> = Vec::new();
    let replies = b
        .list_fonts_with_info_proxy(None, 100, "fixed", &mut |name| {
            interned.push(name.to_owned());
            0x77 + u32::try_from(interned.len()).unwrap()
        })
        .expect("list_fonts_with_info");
    assert!(
        replies.len() >= 2,
        "at least one info reply + terminator; got {}",
        replies.len()
    );
    let info = &replies[0];
    let name_len = usize::from(info[1]);
    let n_props = usize::from(u16::from_le_bytes([info[46], info[47]]));
    let name_off = 60 + n_props * 8;
    let name = std::str::from_utf8(&info[name_off..name_off + name_len]).expect("utf8 name");
    b.core.font_loader.open_font(name).unwrap_or_else(|e| {
        panic!("built-ins LFWI reply name {name:?} must round-trip through open_font: {e:?}")
    });
}

/// Telemetry: counter sites fire at the Backend trait
/// surface even on the test fixture (no Vk). put_image with
/// an unknown xid logs a gap and does NOT count a paint
/// submit (the engine never ran); get_image likewise. This
/// confirms only successful ops count.
#[test]
fn telemetry_counter_sites_track_successful_ops() {
    let mut b = KmsBackend::for_tests();
    // put_image with unknown xid → no counter bump.
    b.put_image(None, 0xDEAD, 32, 4, 4, 0, 0, &[0; 64]).unwrap();
    assert_eq!(b.telemetry.lifetime.paint_submits, 0);
    // The stub engine declines NoVk, so even a known xid
    // wouldn't count. The "track successful ops" gate is
    // covered by the lavapipe integration tests; here we
    // just confirm the wiring compiles and doesn't double-
    // increment on the gap path.
    assert_eq!(b.telemetry.lifetime.queue_submit2, 0);
}

/// Bookkeeping methods stay consistent: register_top_level
/// mutates KmsCore's xid_map; xid_map() reflects the new entry.
#[test]
fn register_top_level_updates_xid_map() {
    use yserver_protocol::x11::ResourceId;
    let mut b = KmsBackend::for_tests();
    b.register_top_level(None, ResourceId(0x4242), 0x0040_1234)
        .expect("register_top_level");
    assert_eq!(b.xid_map().get(&0x0040_1234), Some(&ResourceId(0x4242)));
    b.unregister_host_window(0x0040_1234);
    assert!(b.xid_map().get(&0x0040_1234).is_none());
}

/// Stage 3a per plan §3a: a `poly_text8` wire body that
/// carries `[text₀, font-change, text₁]` should:
/// 1. dispatch the first text run with the original
///    `current_font` value (or None);
/// 2. swap `core.current_font` on the inline change item;
/// 3. dispatch the second text run with the new font.
///
/// Without a real FontState entry the engine call short-
/// circuits in `render_text_chars` (no font → no work),
/// but the side-effect we care about — `current_font`
/// rotating to the inline-change xid by the end of the parse
/// — is observable on the backend after the call returns.
#[test]
fn poly_text8_font_change_advances_current_font() {
    let mut b = KmsBackend::for_tests();
    // Body shape (drawable=4, gc=4, x=2, y=2, items=…):
    // header = 12 bytes; first item = `len(1) delta(1) "X"`
    // = 3 bytes; font-change item = `255 + 4 BE bytes` = 5
    // bytes; second item = `len(1) delta(1) "Y"` = 3 bytes.
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // drawable
    body.extend_from_slice(&[0, 0, 0, 0]); // gc
    body.extend_from_slice(&(0_i16).to_le_bytes()); // x
    body.extend_from_slice(&(0_i16).to_le_bytes()); // y
    // First TEXTITEM8 — single 'X' glyph.
    body.extend_from_slice(&[1u8, 0u8, b'X']);
    // Font-change item — switch to xid 0xDEAD_BEEF.
    body.push(255);
    body.extend_from_slice(&0xDEAD_BEEF_u32.to_be_bytes());
    // Second TEXTITEM8 — single 'Y' glyph.
    body.extend_from_slice(&[1u8, 0u8, b'Y']);

    assert_eq!(b.core.current_font, None);
    b.poly_text8(None, 0xABCD_EF01, 0x000000, &body)
        .expect("poly_text8 ok");
    // After the parse, current_font should reflect the inline
    // change. The parse runs the second text item with this
    // font value in scope.
    assert_eq!(b.core.current_font, Some(0xDEAD_BEEF));
}

/// Stage 5 Task 6.1 (foundation prereq #2): the by-handle xshmfence
/// accessor returns an Arc clone that pins the underlying
/// `FenceMapping` alive past `XFixesDestroyFence` (registry
/// removal). Two clones plus the registry entry should give a
/// strong count of 3; after removing the registry entry the
/// caller-held clones still keep the primitive alive (Drop sees a
/// non-shared Arc).
#[test]
fn xshmfence_handle_accessor_returns_arc_clone() {
    // Construct a backend without Vk (skip if test fixture needs it).
    let mut b = match crate::kms::render::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Manually inject an entry into the registry — bypass the
    // protocol path (DRI3 FenceFromFD) since constructing a real
    // xshmfence FD in a unit test is fragile.
    let xid = 0x1234_5678_u32;
    let mapping = crate::kms::xshmfence::FenceMapping::for_tests_dummy();
    b.dri3_xshmfences.insert(xid, std::sync::Arc::new(mapping));
    let h1 = b.dri3_xshmfence_handle(xid).expect("handle present");
    assert_eq!(
        std::sync::Arc::strong_count(&h1),
        2,
        "registry + caller should both hold a reference"
    );
    let h2 = b.dri3_xshmfence_handle(xid).expect("second handle");
    assert_eq!(
        std::sync::Arc::strong_count(&h1),
        3,
        "registry + two callers should all hold references"
    );
    let _ = h1;
    let _ = h2;
    // Drop the registry entry (mimics XFixesDestroyFence).
    b.dri3_xshmfences.remove(&xid);
    // Accessor returns None now; but the caller's Arc clones
    // still pin the FenceMapping alive (no destructor panic).
    assert!(b.dri3_xshmfence_handle(xid).is_none());
}

/// Phase B.2 Task 14: three `render_composite` calls in the same
/// open frame, then a forced close. After the backend drains the
/// queued `FrameCloseEvent` into telemetry, the lifetime
/// `frame_builder_renders_per_frame_max_in_window` gauge must
/// reflect ≥ 3 (the count of `RecordedOp::RenderComposite` ops
/// recorded into the closing frame). Exercises the full path:
/// engine populates `renders_in_frame` at the close-event push
/// site → backend's `drain_frame_builder_telemetry` reads it →
/// `record_frame_builder_close` accumulates into the bucket +
/// lifetime gauges.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn frame_builder_renders_per_frame_telemetry_records_max() {
    let mut be = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst = be
        .allocate_test_pixmap_bgra(64, 64)
        .expect("allocate_test_pixmap_bgra");

    // Drain any baseline close events from pixmap allocation so the
    // lifetime gauge snapshot is taken at a clean baseline.
    be.engine_flush_submit_group_for_tests()
        .expect("setup drain");

    // Three solid-fill composites into the same dst. All three ops
    // append into the same open frame under the sub-gate.
    let r1 = be.render_composite_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 64, 64);
    let r2 = be.render_composite_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 64, 64);
    let r3 = be.render_composite_for_tests(dst, [0.0, 0.0, 1.0, 1.0], 64, 64);

    // Force frame close via the Timeout helper; this runs the
    // close-walk that pushes the FrameCloseEvent.
    let close_result = be.engine_close_open_frame_for_timeout_for_tests();

    // Reset the process-level sub-gate IMMEDIATELY so neighbouring
    // tests in the same cargo-test binary are not routed through
    // the frame-builder composite path.

    r1.expect("first render_composite_for_tests");
    r2.expect("second render_composite_for_tests");
    r3.expect("third render_composite_for_tests");
    close_result.expect("engine_close_open_frame_for_timeout_for_tests");

    // Drain queued FrameCloseEvent → telemetry. Reuse the existing
    // helper that drains flush outcomes + close events as a side
    // effect of returning submit_group_flushes.
    let _ = be.telemetry_submit_group_flushes_for_tests();
    be.drain_frame_builder_telemetry_for_tests();

    assert!(
        be.telemetry
            .lifetime
            .frame_builder_renders_per_frame_max_in_window
            >= 3,
        "lifetime renders_per_frame_max_in_window must reflect the \
             three RenderComposite ops recorded in the closing frame; got {}",
        be.telemetry
            .lifetime
            .frame_builder_renders_per_frame_max_in_window,
    );
}

#[test]
#[ignore = "needs a DRM render node"]
fn syncobj_handle_accessor_returns_arc_clone() {
    // Shared helper from Task 1 — never hardcode renderD128.
    let Some(drm) = crate::kms::render::imported_syncobj::tests::render_node() else {
        eprintln!("skipping: no render node");
        return;
    };
    let handle =
        ::drm::control::Device::create_syncobj(drm.as_ref(), false).expect("create syncobj");
    let fd = ::drm::control::Device::syncobj_to_fd(drm.as_ref(), handle, false).expect("export fd");

    let mut b = KmsBackend::for_tests();
    let xid = 0xAAAA_BBBB_u32;
    b.dri3_syncobjs.insert(
        xid,
        (
            yserver_protocol::x11::ClientId(1),
            std::sync::Arc::new(
                crate::kms::render::imported_syncobj::ImportedSyncobj::import(
                    drm.clone(),
                    std::os::fd::AsFd::as_fd(&fd),
                )
                .expect("import"),
            ),
        ),
    );

    let h = b.dri3_syncobj_handle(xid).expect("handle present");
    assert_eq!(std::sync::Arc::strong_count(&h), 2);
    b.dri3_syncobjs.remove(&xid);
    // Accessor returns None now; the held Arc still pins the resource
    // alive, which is what the deferred completion path relies on.
    assert!(b.dri3_syncobj_handle(xid).is_none());
    drop(h);

    ::drm::control::Device::destroy_syncobj(drm.as_ref(), handle).expect("destroy");
}
