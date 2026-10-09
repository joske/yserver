use super::*;

// ── DRI3 backfill (Stage 4d.* compositor unblock) ───────────
//
// Ports v1's DRI3 surface to v2. The `for_tests()` fixture
// has no render-node + no Vk, so it exercises the
// "unsupported" branch of every accessor. The Vk-backed
// tests are gated `#[ignore]` and run under `vng` via
// `cargo test -- --ignored`, mirroring the Phase 4.2 hardware
// coverage matrix.

#[test]
fn dri3_implicit_layout_import_is_safe_with_multiple_renderers() {
    let verified = RenderDeviceId::DrmRender(test_device_key(128));

    assert!(
        dri3_import_supported_for_topology(verified, 1),
        "a verified single renderer keeps legacy DRI3",
    );
    assert!(
        dri3_import_supported_for_topology(verified, 2),
        "a verified renderer validates each foreign PRIME buffer's implicit layout",
    );
    assert!(
        dri3_import_supported_for_topology(RenderDeviceId::UnverifiedFallback, 1),
        "the historical one-KMS unverified fallback remains available",
    );
    assert!(
        !dri3_import_supported_for_topology(RenderDeviceId::UnverifiedFallback, 2),
        "an unverified renderer cannot coalesce multiple KMS devices safely",
    );
}

#[test]
fn dri3_capabilities_unsupported_without_vk_returns_unsupported() {
    let b = KmsBackend::for_tests();
    let caps = b.dri3_capabilities();
    // unsupported() sentinel is (0, 0) per the trait_def
    // doc-comment.
    assert_eq!(caps.version, (0, 0), "no Vk → DRI3 reports unsupported");
    assert!(!caps.modifiers);
    assert!(!caps.fence_fd);
    assert!(!caps.syncobj);
}

#[test]
fn dri3_open_errs_when_render_node_unavailable() {
    // for_tests has no selected RenderDevice or render node, so
    // dri3_open must Err out (the SCM_RIGHTS dispatch path
    // then maps it to BadAlloc).
    let mut b = KmsBackend::for_tests();
    let res = b.dri3_open(0x1234);
    assert!(
        res.is_err(),
        "expected Err when no selected render node exists"
    );
}

#[test]
fn dri3_export_pixmap_unknown_xid_errs() {
    // No Vk → first guard fires. With Vk this would still
    // Err because the xid isn't in the store — covered by
    // the Vk-backed test below.
    let mut b = KmsBackend::for_tests();
    let res = b.dri3_export_pixmap(0x4040_0000);
    assert!(res.is_err());
}

#[test]
fn dri3_fd_from_fence_unknown_errs() {
    let mut b = KmsBackend::for_tests();
    assert!(b.dri3_fd_from_fence(0x4040_4040).is_err());
}

#[test]
fn dri3_signal_syncobj_unknown_errs() {
    let mut b = KmsBackend::for_tests();
    assert!(b.dri3_signal_syncobj(0x4040_4040, 1).is_err());
}

#[test]
fn dri3_trigger_fence_unknown_is_ok() {
    // v1's body returns Ok for the unknown-fence case — the
    // VkSemaphore path is server-state-only, no GPU op. v2
    // mirrors.
    let mut b = KmsBackend::for_tests();
    assert!(b.dri3_trigger_fence(0x4040_4040).is_ok());
}

/// DRI3 version follows syncobj support, and syncobj support follows the
/// kernel capability rather than the Vulkan driver. `for_tests()` has no
/// render node, so the capability is false and the version must be 1.3 —
/// which is also the check that would have caught a blacklist creeping back.
#[test]
fn dri3_syncobj_follows_the_kernel_capability() {
    assert_eq!(dri3_version_for(true), (1, 4));
    assert_eq!(dri3_version_for(false), (1, 3));

    let b = KmsBackend::for_tests();
    assert!(
        !b.platform
            .selected_render_device()
            .is_some_and(|device| device.syncobj_timeline),
        "for_tests() has no render node, so the capability must be false",
    );
}

/// Vk-backed: `dri3_import_pixmap` rejects unsupported
/// (depth, bpp) combinations with a non-empty error before
/// touching the dma-buf fd. Exercises the guard above the
/// `import_dmabuf` call. Vk-attached so we hit the second
/// #138 regression, at the metadata boundary that actually broke.
///
/// An imported pixmap must be handed back to the client **as the
/// client described it**. Two separate lies used to live here:
/// a legacy `PixmapFromBuffer` was recorded as explicit LINEAR, and
/// every export re-derived its answer from our own VkImage rather
/// than from the client's buffer. Chrome imports a TILED VA-API
/// frame through the legacy request and then asks for it straight
/// back, so both lies reached it and it sampled its own frame wrong.
///
/// The contract asserted here:
///   - an implicit import reports `DRM_FORMAT_MOD_INVALID`, never
///     LINEAR -- "I was not told" is the honest answer, and it is
///     what lets the client resolve the layout itself;
///   - an explicit import reports back that same modifier;
///   - stride and offset survive the round trip unchanged.
#[test]
#[ignore = "needs a Vulkan ICD that can export dma-bufs (not lavapipe)"]
fn dri3_imported_pixmap_exports_the_clients_own_description() {
    use yserver_core::backend::Dri3ImportModifier;
    const INVALID: u64 = crate::kms::vk::dri3::DRM_FORMAT_MOD_INVALID;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: no Vk: {e}");
            return;
        }
    };
    // A real dma-buf from the SAME device the backend renders on:
    // create a server pixmap and export it. Sourcing one from a
    // /dev/dri node instead risks allocating on the wrong GPU on a
    // dual-GPU host, and skipping when a node is missing made an
    // earlier version of this test pass vacuously.
    let (w, h) = (256u16, 64u16);
    let seed = b
        .create_pixmap(None, 32, w, h)
        .expect("fixture: create_pixmap with a live Vk context");
    // The real precondition is not "Vulkan initialised" but "this
    // device can hand out an exportable dma-buf". CI runs the
    // ignored tests on lavapipe, which initialises fine and then
    // cannot allocate exportable storage -- that is a legitimate
    // "not here", not a failure.
    //
    // Everything else IS a failure. Skipping on any error is how an
    // earlier version of this test passed vacuously, and the point of
    // the narrow match is to keep that door shut.
    let seed_export = match b.dri3_export_pixmap_buffers(seed.as_raw()) {
        Ok(e) => e,
        Err(e) if e.to_string().contains("ERROR_FORMAT_NOT_SUPPORTED") => {
            eprintln!("skip: ICD cannot export dma-bufs (lavapipe on CI): {e}");
            return;
        }
        Err(e) => panic!("fixture: export the seed pixmap: {e}"),
    };
    assert!(
        seed_export.size > 0 && seed_export.stride > 0,
        "fixture: seed export must describe a real buffer, got size={} stride={}",
        seed_export.size,
        seed_export.stride,
    );
    let stride = seed_export.stride;
    let seed_modifier = seed_export.modifier;

    // The legacy request states a size on the wire; an export must
    // report that number back verbatim.
    //
    // Deliberately NOT the seed's own size: the fallback path derives
    // the same number from the Vulkan layout, so reusing it makes the
    // assertion pass whether or not the stated size is honoured. A
    // distinguishable value is what gives it teeth. A client would not
    // normally overstate its buffer, but the contract is "report what
    // the client said", and nothing here consumes the buffer.
    let stated_size = seed_export.size + 4096;
    for (case, requested, expected, expected_size) in [
        (
            "implicit",
            Dri3ImportModifier::Implicit { size: stated_size },
            INVALID,
            stated_size,
        ),
        (
            // No size on the PixmapFromBuffers wire, so this one
            // legitimately falls back to the Vulkan layout.
            "explicit",
            Dri3ImportModifier::Explicit(seed_modifier),
            seed_modifier,
            seed_export.size,
        ),
    ] {
        let fd = seed_export.fd.try_clone().expect("dup the seed dma-buf");
        let handle = b
            .dri3_import_pixmap(fd, w, h, stride, 0, requested, 32, 32)
            .unwrap_or_else(|e| panic!("{case}: import failed: {e}"));
        let export = b
            .dri3_export_pixmap_buffers(handle.as_raw())
            .unwrap_or_else(|e| panic!("{case}: export failed: {e}"));

        assert_eq!(
            export.modifier, expected,
            "{case}: exported modifier must be what the client's buffer is described by. \
                 Reporting LINEAR (0) for an unnamed layout is #138 -- the client believes it \
                 and samples a tiled buffer as linear",
        );
        assert_eq!(
            export.stride, stride,
            "{case}: the client's own stride must survive the round trip, not be \
                 re-derived from our VkImage",
        );
        assert_eq!(export.offset, 0, "{case}: offset must round trip");
        assert_eq!(
            export.size, expected_size,
            "{case}: the client's stated buffer size must be reported verbatim. \
                 Measuring it from the fd instead reports 0 on a failed seek, and moves \
                 the client's file offset, since a dup'd SCM_RIGHTS fd shares its \
                 open-file description",
        );
    }
}

/// arm (the Vk branch).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn dri3_import_pixmap_rejects_unsupported_depth_bpp() {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: no Vk: {e}");
            return;
        }
    };
    // Synthesise an arbitrary fd — depth=8 trips the guard
    // before the fd is consumed, so any openable file works.
    let f = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .expect("open /dev/null");
    let raw = f.into_raw_fd();
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let res = b.dri3_import_pixmap(
        fd,
        16,
        16,
        64,
        0,
        yserver_core::backend::Dri3ImportModifier::Explicit(0),
        8,
        8,
    );
    assert!(
        res.is_err(),
        "depth=8 bpp=8 is outside Phase 4.2 RGB single-plane scope",
    );
}

/// Vk-backed: `dri3_supported_modifiers` returns at least
/// LINEAR (0) on the screen side for depth-32/bpp-32. Lavapipe
/// reports LINEAR; Venus reports LINEAR + tile modifiers; we
/// only assert LINEAR is present (the conservative invariant).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn dri3_supported_modifiers_includes_linear_with_vk() {
    let b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: no Vk: {e}");
            return;
        }
    };
    let (window, screen) = b.dri3_supported_modifiers(0, 24, 32);
    assert!(
        window.contains(&0),
        "window modifiers always include LINEAR (Phase 4.1 scanout policy)",
    );
    assert!(
        screen.contains(&0),
        "screen modifiers always include LINEAR (fallback row of the design matrix)",
    );
}

/// xshmfence-path of `dri3_fence_from_fd`: mmap an xshmfence
/// (synthesised via `memfd_create` + `ftruncate`), feed the
/// fd in, assert it landed in `dri3_xshmfences` (not
/// `dri3_sync_resources`) and that `trigger()` flips the
/// state to signalled.
#[test]
fn dri3_fence_from_fd_xshmfence_path_triggers() {
    // The xshmfence module exposes the C alloc/map helpers;
    // build a fresh shm fd via `xshmfence_alloc_shm` directly
    // through libc-equivalent shape (memfd_create). To keep
    // the test self-contained without pulling libxshmfence
    // alloc, we synthesise a memfd that's at least page-sized
    // and let `FenceMapping::map` mmap it — libxshmfence's
    // map_shm only requires the fd be at least one page.
    use std::os::fd::{FromRawFd, OwnedFd};
    #[cfg(target_os = "linux")]
    let raw_result =
        unsafe { libc::syscall(libc::SYS_memfd_create, c"yserver_dri3_test".as_ptr(), 0u32) };
    #[cfg(not(target_os = "linux"))]
    let raw_result = i64::from(unsafe { libc::memfd_create(c"yserver_dri3_test".as_ptr(), 0) });
    if raw_result < 0 {
        eprintln!("skip: memfd_create unavailable");
        return;
    }
    let raw = i32::try_from(raw_result).expect("fd fits i32");
    // Size the memfd to one page so map_shm succeeds.
    let page_raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page: libc::off_t = if page_raw > 0 {
        page_raw as libc::off_t
    } else {
        4096
    };
    if unsafe { libc::ftruncate(raw, page) } != 0 {
        unsafe { libc::close(raw) };
        eprintln!("skip: ftruncate failed");
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut b = KmsBackend::for_tests();
    let fence_xid: u32 = 0x4040_1111;
    b.dri3_fence_from_fd(fence_xid, fd)
        .expect("xshmfence import");
    assert!(
        b.dri3_xshmfences.contains_key(&fence_xid),
        "xshmfence path stores under dri3_xshmfences",
    );
    assert!(
        !b.dri3_sync_resources.contains_key(&fence_xid),
        "xshmfence path must NOT also populate dri3_sync_resources",
    );
    // Inspect the mapping's pre-trigger state.
    let mapping = b.dri3_xshmfences.get(&fence_xid).expect("present");
    let pre = mapping.query();
    // Trigger via the public trait surface.
    b.dri3_trigger_fence(fence_xid).expect("trigger ok");
    let post = b
        .dri3_xshmfences
        .get(&fence_xid)
        .expect("still present")
        .query();
    assert_eq!(post, 1, "after trigger, xshmfence query() == 1");
    // Defensive: pre and post differ in the expected direction.
    assert_ne!(pre, post, "trigger() should have changed the fence state");
}

/// An imported xshmfence is shared with the client, which triggers and
/// resets it in memory itself. The backend's fence state is that
/// memory (Xorg `miSyncShmFenceCheckTriggered` → `xshmfence_query`),
/// ResetFence resets it, and DestroyFence triggers it before
/// unmapping (`miSyncShmScreenDestroyFence`). A second mapping of the
/// same memfd plays the client.
#[test]
fn dri3_shm_fence_state_is_the_shared_memory() {
    use std::os::fd::{AsFd as _, FromRawFd, OwnedFd};
    use yserver_core::backend::Backend as _;
    #[cfg(target_os = "linux")]
    let raw_result =
        unsafe { libc::syscall(libc::SYS_memfd_create, c"yserver_shm_fence".as_ptr(), 0u32) };
    #[cfg(not(target_os = "linux"))]
    let raw_result = i64::from(unsafe { libc::memfd_create(c"yserver_shm_fence".as_ptr(), 0) });
    assert!(raw_result >= 0, "memfd_create");
    let raw = i32::try_from(raw_result).expect("fd fits i32");
    let page = libc::off_t::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
    assert_eq!(unsafe { libc::ftruncate(raw, page) }, 0, "ftruncate");
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let client = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()).expect("client map");
    let mut b = KmsBackend::for_tests();
    let fence: u32 = 0x4040_2222;
    assert_eq!(b.dri3_fence_triggered(fence), None, "not imported yet");
    b.dri3_fence_from_fd(fence, fd).expect("xshmfence import");
    assert_eq!(b.dri3_fence_triggered(fence), Some(false));

    client.trigger();
    assert_eq!(b.dri3_fence_triggered(fence), Some(true), "client trigger");
    client.reset();
    assert_eq!(b.dri3_fence_triggered(fence), Some(false), "client reset");

    b.dri3_trigger_fence(fence).expect("trigger");
    assert_eq!(client.query(), 1);
    b.dri3_reset_fence(fence);
    assert_eq!(client.query(), 0, "ResetFence resets the memory");

    b.dri3_destroy_fence(fence);
    assert_eq!(client.query(), 1, "DestroyFence triggers before unmapping");
    assert_eq!(b.dri3_fence_triggered(fence), None, "unmapped");
    assert!(!b.dri3_xshmfences.contains_key(&fence));
}

/// Fences and syncobjs are different X resource types with different
/// backing primitives. Each resolver must see only its own registry: before
/// the split they shared one map, so FDFromFence on a syncobj xid resolved
/// and half-worked.
#[test]
#[ignore = "needs a DRM render node"]
fn each_resolver_sees_only_its_own_registry() {
    // Shared helper from Task 1 — never hardcode renderD128, see its doc
    // comment for why (multi-GPU hosts pick the wrong device silently).
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

    // The syncobj resolver finds it.
    assert!(
        b.dri3_syncobj_handle(xid).is_some(),
        "syncobj registry must resolve a syncobj xid",
    );
    // The fence resolver must NOT, and must say so as an unknown fence
    // rather than tripping over some other gate first.
    let err = b
        .dri3_fd_from_fence(xid)
        .expect_err("FDFromFence must not resolve a syncobj xid");
    assert!(
        err.to_string().contains("unknown fence"),
        "expected an unknown-fence error, got: {err}",
    );

    ::drm::control::Device::destroy_syncobj(drm.as_ref(), handle).expect("destroy");
}

/// ImportSyncobj no longer needs Vulkan. for_tests() has no render node,
/// so feed it a bogus one and assert the ioctl — not the device guard —
/// is what fails.
#[test]
fn dri3_import_syncobj_errs_without_a_usable_drm_handle() {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .expect("open /dev/null");
    // SAFETY: we own this fd via the OpenOptions handle and re-wrap it
    // directly; the OwnedFd's Drop closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(f.into_raw_fd()) };
    let mut b = KmsBackend::for_tests();
    b.platform
        .render_devices
        .push(crate::kms::render::platform::RenderDevice {
            id: crate::kms::render::platform::RenderDeviceId::UnverifiedFallback,
            physical_device: ash::vk::PhysicalDevice::null(),
            selector: crate::kms::vk::device::VulkanDeviceSelector::for_tests(0x40),
            advertised_primary_node: None,
            advertised_render_node: None,
            render_node: None,
            render_node_device: Some(std::sync::Arc::new(
                crate::drm::Device::open_render_node("/dev/null").expect("open /dev/null"),
            )),
            syncobj_timeline: false,
        });
    b.platform.selected_render_device =
        Some(crate::kms::render::platform::RenderDeviceId::UnverifiedFallback);
    assert!(
        b.dri3_import_syncobj(yserver_protocol::x11::ClientId(1), 0x4040_3333, fd,)
            .is_err(),
        "importing a non-syncobj fd must Err",
    );
}
