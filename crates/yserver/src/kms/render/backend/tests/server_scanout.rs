//! Candidate tracking, rotations and caps for direct scanout of server-owned
//! Present sources (plan Task 6), driven through the M0 observation with a
//! fake promoter.

use super::{
    present_scanout::{bind_test_randr_crtc, seed_cow_stage},
    *,
};
use crate::kms::render::{
    backend::{
        DirectScope, RotationState, SERVER_SCANOUT_PROMOTE_BURST, SERVER_SCANOUT_PROMOTE_REFILL,
        ScanoutM1ProbeEntry, ScanoutSourceLayout, ServerScanoutDescriptor,
        ServerScanoutPromoteError, ServerScanoutPromotion, ServerScanoutState,
    },
    store::{DrawableId, DrawableKind, Storage},
};
use std::{cell::Cell, rc::Rc, time::Instant};
use yserver_core::backend::{Backend, CompletedPresentEvent, PresentScanoutCandidate, PresentWake};

const STAGE: u32 = 0x0060_0001;
/// Sources whose xid carries this bit fail the fake promoter.
const FAILS: u32 = 0x0000_8000;

fn fake_framebuffer_handle(id: DrawableId) -> ::drm::control::framebuffer::Handle {
    ::drm::control::from_u32(0x9000 + u32::try_from(id.as_u64()).expect("small test id"))
        .expect("nonzero test handle")
}

/// Accepts every source except those whose xid has [`FAILS`]; never touches
/// storage.
fn fake_promoter(
    b: &mut KmsBackend,
    id: DrawableId,
) -> Result<ServerScanoutPromotion, ServerScanoutPromoteError> {
    let drawable = b.store.get(id).expect("promoted source is alive");
    if drawable.xid & FAILS != 0 {
        return Err(ServerScanoutPromoteError::ProbeRejected("scripted".into()));
    }
    let extent = drawable.storage.extent;
    let fd = std::fs::File::open("/dev/null").expect("open /dev/null");
    Ok(ServerScanoutPromotion {
        framebuffer: crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
            fake_framebuffer_handle(id),
        ),
        descriptor: ServerScanoutDescriptor {
            fd: fd.into(),
            layout: ScanoutSourceLayout {
                fourcc: 0x3432_5258,
                vk_format: ash::vk::Format::B8G8R8A8_UNORM,
                depth: 24,
                bpp: 32,
                width: u16::try_from(extent.width).expect("root width"),
                height: u16::try_from(extent.height).expect("root height"),
                plane_count: 1,
                offset: 0,
                pitch: extent.width * 4,
                modifier: 0,
            },
        },
    })
}

/// The kernel's `struct drm_mode_modeinfo` (`drm_mode.h`), which
/// `drm::control::Mode` wraps transparently.
#[repr(C)]
struct DrmModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

/// A mode whose `size()` is `(width, height)`; the fixture's outputs carry a
/// zeroed mode, which M1's root-tiling check rejects.
fn mode_of_size(width: u16, height: u16) -> ::drm::control::Mode {
    const _: () =
        assert!(std::mem::size_of::<DrmModeInfo>() == std::mem::size_of::<::drm::control::Mode>());
    let info = DrmModeInfo {
        clock: 0,
        hdisplay: width,
        hsync_start: 0,
        hsync_end: 0,
        htotal: 0,
        hskew: 0,
        vdisplay: height,
        vsync_start: 0,
        vsync_end: 0,
        vtotal: 0,
        vscan: 0,
        vrefresh: 0,
        flags: 0,
        type_: 0,
        name: [0; 32],
    };
    // SAFETY: `Mode` is `repr(transparent)` over `drm_mode_modeinfo`, whose
    // layout `DrmModeInfo` reproduces field for field; sizes are asserted.
    let mode: ::drm::control::Mode = unsafe { std::mem::transmute(info) };
    assert_eq!(mode.size(), (width, height));
    mode
}

/// A backend whose root is covered by a COW stage, with the fake promoter
/// and a test-driven clock. Returns the clock offset cell.
fn backend() -> (KmsBackend, Rc<Cell<std::time::Duration>>) {
    let mut b =
        KmsBackend::for_tests_with_direct_submitter(KmsBackend::submit_direct_frame_for_tests);
    seed_cow_stage(&mut b, STAGE);
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    b.platform.outputs[0].output.mode = mode_of_size(w, h);
    // No scene in this fixture: a hidden cursor is the eligible one.
    Backend::set_cursor_hidden(&mut b, true);
    let crtc_id = u32::from(b.platform.outputs[0].output.crtc);
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    b.pstate.server_scanout_promoter = fake_promoter;
    let offset = Rc::new(Cell::new(std::time::Duration::ZERO));
    let base = Instant::now();
    let clock_offset = Rc::clone(&offset);
    b.for_tests_set_monotonic_clock(Rc::new(move || base + clock_offset.get()));
    (b, offset)
}

/// A root-sized stage window under the COW.
fn add_stage(b: &mut KmsBackend, xid: u32) -> DrawableId {
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let id = seed_window(b, xid, Some(cow), 0, 0);
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    let geometry = b.windows.get_mut(&xid).expect("stage geometry");
    geometry.width = w;
    geometry.height = h;
    id
}

fn pixmap(b: &mut KmsBackend, xid: u32, depth: u8, width: u32, height: u32) -> DrawableId {
    b.store
        .allocate(
            xid,
            DrawableKind::Pixmap,
            depth,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D { width, height },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("allocate test Present source")
}

fn root_pixmap(b: &mut KmsBackend, xid: u32) -> DrawableId {
    let (w, h) = (u32::from(b.platform.fb_w), u32::from(b.platform.fb_h));
    pixmap(b, xid, 24, w, h)
}

fn candidate(b: &KmsBackend, source_xid: u32, dst_xid: u32) -> PresentScanoutCandidate {
    let crtc_id = u32::from(b.platform.outputs[0].output.crtc);
    PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id: 1,
        crtc_id,
        crtc_epoch: b.present_crtc_clock_epoch(crtc_id),
        src_pixmap_xid: source_xid,
        dst_window_xid: dst_xid,
        variable_refresh: false,
        src_host_xid: source_xid,
        paint_dst_host_xid: dst_xid,
        completion_dst_host_xid: dst_xid,
        src_width: b.platform.fb_w,
        src_height: b.platform.fb_h,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    }
}

/// The M0 observation of one Present, as the core reports it before the
/// Present is executed.
fn observe(b: &mut KmsBackend, source_xid: u32, dst_xid: u32) {
    let candidate = candidate(b, source_xid, dst_xid);
    Backend::note_present_scanout_candidate(b, candidate);
}

/// Observe, then try the M2 path; returns whether the Present flipped.
fn present(b: &mut KmsBackend, present_id: u64, source_xid: u32, dst_xid: u32) -> bool {
    let candidate = PresentScanoutCandidate {
        present_id,
        ..candidate(b, source_xid, dst_xid)
    };
    Backend::note_present_scanout_candidate(b, candidate);
    let event = CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: u32::try_from(present_id).expect("test serial"),
        host_xid: source_xid,
        dst_host_xid: dst_xid,
        options: 0,
        present_id,
        window_generation: 1,
        crtc_id: candidate.crtc_id,
        crtc_epoch: candidate.crtc_epoch,
        msc_offset: 0,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };
    Backend::try_present_direct(b, candidate, event).expect("try the M2 path")
}

fn attempts(b: &KmsBackend) -> u64 {
    b.pstate.scanout_m0.server.promote_attempts
}

fn tokens(b: &KmsBackend) -> u32 {
    b.pstate.server_scanout.tokens.tokens
}

fn rotation_state(b: &KmsBackend, dst_xid: u32) -> Option<RotationState> {
    let dest = b.store.lookup(dst_xid)?;
    b.pstate
        .server_scanout
        .rotations
        .get(&dest)
        .map(|rotation| rotation.state)
}

fn is_promoted(b: &KmsBackend, id: DrawableId) -> bool {
    b.pstate.server_scanout.is_promoted(id)
}

fn accepted_framebuffer(
    b: &KmsBackend,
    id: DrawableId,
) -> Option<::drm::control::framebuffer::Handle> {
    b.pstate
        .scanout_m1
        .entries
        .get(&(id, DirectScope::Root))
        .and_then(ScanoutM1ProbeEntry::framebuffer)
        .map(crate::drm::modeset::DirectScanoutProbeFramebuffer::handle)
}

#[test]
fn single_buffer_rotation_promotes_on_its_second_present() {
    let (mut b, _) = backend();
    let a = root_pixmap(&mut b, 0x0061_0001);

    observe(&mut b, 0x0061_0001, STAGE);
    assert_eq!(b.pstate.scanout_m0.server.candidates, 1);
    assert_eq!(attempts(&b), 0);
    assert!(!is_promoted(&b, a));

    observe(&mut b, 0x0061_0001, STAGE);
    assert_eq!(attempts(&b), 1);
    assert!(is_promoted(&b, a));
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a))
    );
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Complete));

    observe(&mut b, 0x0061_0001, STAGE);
    assert_eq!(attempts(&b), 1, "a promoted source is never promoted again");
    assert_eq!(tokens(&b), SERVER_SCANOUT_PROMOTE_BURST - 1);
}

/// Presents `sources` in order to `dst_xid` through observation and the M2
/// path; returns how many flipped.
fn present_all(b: &mut KmsBackend, first_id: u64, sources: &[u32], dst_xid: u32) -> usize {
    sources
        .iter()
        .zip(first_id..)
        .filter(|(source, id)| present(b, *id, **source, dst_xid))
        .count()
}

#[test]
fn marco_double_buffer_promotes_both_on_its_fourth_present() {
    let (mut b, _) = backend();
    let (a_xid, b_xid) = (0x0061_0011, 0x0061_0012);
    let a = root_pixmap(&mut b, a_xid);
    let b_id = root_pixmap(&mut b, b_xid);

    for (n, xid) in [a_xid, b_xid, a_xid].into_iter().enumerate() {
        observe(&mut b, xid, STAGE);
        assert_eq!(
            attempts(&b),
            0,
            "nothing promotes before the 4th Present ({n})"
        );
    }
    observe(&mut b, b_xid, STAGE);
    assert_eq!(attempts(&b), 2);
    assert!(is_promoted(&b, a) && is_promoted(&b, b_id));
    assert_eq!(tokens(&b), SERVER_SCANOUT_PROMOTE_BURST - 2);
    let dest = b.store.lookup(STAGE).expect("stage");
    assert!(b.pstate.server_scanout.rotation_complete(dest, a));
    assert!(b.pstate.server_scanout.rotation_complete(dest, b_id));
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a))
    );
    assert_eq!(
        accepted_framebuffer(&b, b_id),
        Some(fake_framebuffer_handle(b_id))
    );

    // Past M2 probation, A and B alternate direct flips with no unflip.
    // No scene to sync cursor planes with: treat them as bound.
    b.pstate.direct.root_mut().cursor_bound_all = true;
    let sources: Vec<u32> = [a_xid, b_xid].repeat(4);
    assert_eq!(present_all(&mut b, 10, &sources, STAGE), 1, "the 8th flips");
    for (n, xid) in [a_xid, b_xid].repeat(3).into_iter().enumerate() {
        let id = 20 + u64::try_from(n).expect("small");
        assert!(present(&mut b, id, xid, STAGE), "Present {id} flips");
        retire_flip(&mut b);
    }
    retire_flip(&mut b);
    assert_eq!(
        b.pstate.scanout_m0.server.unflip_reasons,
        Default::default()
    );
    let flipped: Vec<_> = b
        .pstate
        .direct
        .root()
        .completed
        .iter()
        .map(|event| event.present_id)
        .collect();
    assert_eq!(flipped, [17, 20, 21, 22, 23, 24, 25]);
    assert_eq!(attempts(&b), 2);

    // Control: an imported root-sized source with an accepted probe entry
    // flips on the same destination.
    let c_xid = 0x0061_0013;
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    let mut storage = Storage::for_tests_null(
        ash::vk::Extent2D {
            width: u32::from(w),
            height: u32::from(h),
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    storage.imported_dmabuf = Some(imported_dmabuf_for_test(w, h));
    let c = b
        .store
        .allocate(c_xid, DrawableKind::Pixmap, 24, false, storage)
        .expect("imported source");
    b.pstate.scanout_m1.entries.insert(
        (c, DirectScope::Root),
        ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(fake_framebuffer_handle(
                c,
            )),
        ),
    );
    assert!(present(&mut b, 30, c_xid, STAGE));
}

/// Retire the pending direct flip on output 0.
fn retire_flip(b: &mut KmsBackend) {
    let event = b
        .pstate
        .direct
        .root()
        .pending
        .as_ref()
        .expect("a pending direct flip")
        .accepted_event_id;
    let clock = yserver_core::backend::PresentClockSample {
        msc: 4,
        ust: 400,
        source: yserver_core::backend::PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, event, clock));
}

#[test]
fn second_buffer_failure_fails_the_rotation_and_refunds_its_token() {
    let (mut b, _) = backend();
    let (a_xid, b_xid) = (0x0061_0021, 0x0061_0022 | FAILS);
    let a = root_pixmap(&mut b, a_xid);
    let b_id = root_pixmap(&mut b, b_xid);
    let storage_before = {
        let s = &b.store.get(b_id).expect("B").storage;
        (s.image, s.memory)
    };

    assert_eq!(
        present_all(&mut b, 1, &[a_xid, b_xid, a_xid, b_xid], STAGE),
        0
    );
    assert_eq!(attempts(&b), 2);
    assert!(is_promoted(&b, a), "a successful swap is never undone");
    assert!(matches!(
        b.pstate.server_scanout.sources.get(&b_id),
        Some(ServerScanoutState::Failed(_))
    ));
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Failed));
    assert_eq!(
        tokens(&b),
        SERVER_SCANOUT_PROMOTE_BURST - 1,
        "one spent, one refunded"
    );
    let s = &b.store.get(b_id).expect("B").storage;
    assert_eq!((s.image, s.memory), storage_before);
    assert!(!s.is_exportable());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(b_id, DirectScope::Root))
    );

    let sources: Vec<u32> = [a_xid, b_xid].repeat(6);
    assert_eq!(present_all(&mut b, 10, &sources, STAGE), 0);
    assert_eq!(attempts(&b), 2, "a failed source is never retried");
    assert!(!b.pstate.direct.root().active());
    assert!(b.pstate.direct.root().unflip_reason.is_none());
    assert!(b.pstate.scanout_m0.server.unflip_reasons.is_empty());
}

#[test]
fn third_source_resets_the_rotation_and_unflips() {
    let (mut b, _) = backend();
    let (a_xid, b_xid, c_xid) = (0x0061_0031, 0x0061_0032, 0x0061_0033);
    let a = root_pixmap(&mut b, a_xid);
    let _ = root_pixmap(&mut b, b_xid);
    let c = root_pixmap(&mut b, c_xid);
    for xid in [a_xid, b_xid, a_xid, b_xid] {
        observe(&mut b, xid, STAGE);
    }
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Complete));
    // Stand-in for a live direct frame of A.
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    retain_direct_frame_from_source_test(&mut b, a_xid, STAGE, w, h);
    assert!(b.pstate.direct.root().active());

    observe(&mut b, c_xid, STAGE);
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Observing));
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("server_source_rotation_changed")
    );
    let dest = b.store.lookup(STAGE).expect("stage");
    assert_eq!(
        b.pstate.server_scanout.rotations[&dest].members,
        vec![(c, 1)]
    );

    // A new rotation holding the already promoted A pays only for C.
    for xid in [a_xid, c_xid, a_xid] {
        observe(&mut b, xid, STAGE);
    }
    assert_eq!(attempts(&b), 3);
    assert_eq!(tokens(&b), SERVER_SCANOUT_PROMOTE_BURST - 3);
    assert!(b.pstate.server_scanout.rotation_complete(dest, a));
    assert!(b.pstate.server_scanout.rotation_complete(dest, c));
}

#[test]
fn retiring_a_member_leaves_complete_and_unflips() {
    let (mut b, _) = backend();
    let (a_xid, b_xid) = (0x0061_0041, 0x0061_0042);
    let a = root_pixmap(&mut b, a_xid);
    let b_id = root_pixmap(&mut b, b_xid);
    for xid in [a_xid, b_xid, a_xid, b_xid] {
        observe(&mut b, xid, STAGE);
    }
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    retain_direct_frame_from_source_test(&mut b, b_xid, STAGE, w, h);

    b.store_decref_with_invalidate(a);
    assert!(b.store.get(a).is_none());
    assert!(!b.pstate.server_scanout.sources.contains_key(&a));
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Observing));
    let dest = b.store.lookup(STAGE).expect("stage");
    assert!(!b.pstate.server_scanout.rotation_complete(dest, b_id));
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("server_source_retired")
    );
    assert_eq!(b.pstate.server_scanout.live(), 1);
}

#[test]
fn retiring_the_destination_removes_its_rotation() {
    let (mut b, _) = backend();
    let stage2 = 0x0060_0002;
    let dest = add_stage(&mut b, stage2);
    let a_xid = 0x0061_0051;
    root_pixmap(&mut b, a_xid);
    observe(&mut b, a_xid, stage2);
    assert!(b.pstate.server_scanout.rotations.contains_key(&dest));
    b.store_decref_with_invalidate(dest);
    assert!(!b.pstate.server_scanout.rotations.contains_key(&dest));
}

#[test]
fn ineligible_sources_and_targets_never_promote() {
    let (mut b, _) = backend();
    let (w, h) = (u32::from(b.platform.fb_w), u32::from(b.platform.fb_h));
    let observe_twice = |b: &mut KmsBackend, source_xid: u32, dst_xid: u32, x_off: i16| {
        let candidate = PresentScanoutCandidate {
            x_off,
            ..candidate(b, source_xid, dst_xid)
        };
        for _ in 0..2 {
            Backend::note_present_scanout_candidate(b, candidate);
        }
    };

    // Depth 32.
    pixmap(&mut b, 0x0062_0001, 32, w, h);
    observe_twice(&mut b, 0x0062_0001, STAGE, 0);
    // Not root-sized.
    pixmap(&mut b, 0x0062_0002, 24, w / 2, h);
    observe_twice(&mut b, 0x0062_0002, STAGE, 0);
    // Nonzero x_off.
    root_pixmap(&mut b, 0x0062_0003);
    observe_twice(&mut b, 0x0062_0003, STAGE, 1);
    // Unredirected target: a root-sized top-level outside the COW.
    let top = 0x0060_0010;
    seed_window(&mut b, top, None, 0, 0);
    let geometry = b.windows.get_mut(&top).expect("top-level");
    geometry.width = b.platform.fb_w;
    geometry.height = b.platform.fb_h;
    b.core.top_level_order.push(top);
    root_pixmap(&mut b, 0x0062_0004);
    observe_twice(&mut b, 0x0062_0004, top, 0);
    // Bordered target.
    let bordered = 0x0060_0011;
    let bordered_id = add_stage(&mut b, bordered);
    b.windows.get_mut(&bordered).expect("stage").border_width = 2;
    b.store.set_content_offset(bordered_id, 2);
    assert!(
        b.resolve_paint_target(bordered)
            .expect("bordered stage")
            .has_border_clip()
    );
    root_pixmap(&mut b, 0x0062_0005);
    observe_twice(&mut b, 0x0062_0005, bordered, 0);
    // GLX-bound: an export entry exists.
    let glx = root_pixmap(&mut b, 0x0062_0006);
    b.pstate.exported_dmabufs.insert(
        glx,
        crate::kms::render::backend::ExportedBacking {
            fd: None,
            glx_refs: 1,
            lifetime_ref_held: false,
            backing_id: glx,
            backing: yserver_core::backend::PixmapHandle::from_raw(0x0062_0006).expect("nonzero"),
            lifetime_via_alias: false,
        },
    );
    observe_twice(&mut b, 0x0062_0006, STAGE, 0);
    // DRI3-exported: a client holds an fd.
    let dri3 = root_pixmap(&mut b, 0x0062_0007);
    let fd: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").expect("fd").into();
    b.store.set_exported_sync_fd(dri3, std::sync::Arc::new(fd));
    observe_twice(&mut b, 0x0062_0007, STAGE, 0);
    assert_eq!(b.pstate.scanout_m0.server.reject_exported, 4);

    // A non-empty root overlay closes the gate for an otherwise eligible one.
    root_pixmap(&mut b, 0x0062_0008);
    b.scene.root_overlay_toggle(
        yserver_protocol::x11::ClientId(5),
        0x00ff_ffff,
        &[ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 4,
                height: 4,
            },
        }],
    );
    observe_twice(&mut b, 0x0062_0008, STAGE, 0);

    assert_eq!(b.pstate.scanout_m0.server.candidates, 0);
    assert_eq!(attempts(&b), 0);
    assert!(b.pstate.server_scanout.rotations.is_empty());

    // Control: the overlay was the only reason the last one was refused.
    b.scene.root_overlay.clear();
    observe_twice(&mut b, 0x0062_0008, STAGE, 0);
    assert_eq!(attempts(&b), 1);
}

#[test]
fn failure_cap_holds_until_the_topology_changes() {
    let (mut b, _) = backend();
    for n in 0..5u32 {
        let stage = 0x0060_0020 + n;
        add_stage(&mut b, stage);
        let source = (0x0063_0000 + n) | FAILS;
        root_pixmap(&mut b, source);
        observe(&mut b, source, stage);
        observe(&mut b, source, stage);
    }
    assert_eq!(attempts(&b), 4);
    assert_eq!(b.pstate.scanout_m0.server.promote_fail, 4);
    assert_eq!(b.pstate.scanout_m0.server.reject_cap_fail, 1);
    assert_eq!(
        tokens(&b),
        SERVER_SCANOUT_PROMOTE_BURST,
        "failures spend no tokens"
    );

    b.platform.outputs[0].output.picked.flags ^= 1;
    observe(&mut b, 0x0063_0004 | FAILS, 0x0060_0024);
    assert_eq!(attempts(&b), 5, "a new topology epoch reopens the cap");
}

#[test]
fn success_bucket_refills_with_time_not_topology() {
    let (mut b, clock) = backend();
    let promote_and_free = |b: &mut KmsBackend, xid: u32| {
        let id = root_pixmap(b, xid);
        observe(b, xid, STAGE);
        observe(b, xid, STAGE);
        let promoted = is_promoted(b, id);
        b.store_decref_with_invalidate(id);
        promoted
    };
    for n in 0..SERVER_SCANOUT_PROMOTE_BURST {
        assert!(
            promote_and_free(&mut b, 0x0064_0000 + n),
            "burst promotion {n}"
        );
    }
    assert_eq!(tokens(&b), 0);

    let ninth_xid = 0x0064_0100;
    let ninth = root_pixmap(&mut b, ninth_xid);
    observe(&mut b, ninth_xid, STAGE);
    observe(&mut b, ninth_xid, STAGE);
    assert!(!is_promoted(&b, ninth));
    assert_eq!(b.pstate.scanout_m0.server.reject_cap_success, 1);

    b.platform.outputs[0].output.picked.flags ^= 1;
    clock.set(SERVER_SCANOUT_PROMOTE_REFILL - std::time::Duration::from_secs(1));
    observe(&mut b, ninth_xid, STAGE);
    assert!(!is_promoted(&b, ninth), "a topology change refills nothing");

    clock.set(SERVER_SCANOUT_PROMOTE_REFILL);
    observe(&mut b, ninth_xid, STAGE);
    assert!(is_promoted(&b, ninth));
    assert_eq!(tokens(&b), 0);
    b.store_decref_with_invalidate(ninth);

    // One token cannot cover a two-buffer rotation: both stay on Copy.
    clock.set(SERVER_SCANOUT_PROMOTE_REFILL * 2);
    let (a_xid, b_xid) = (0x0064_0201, 0x0064_0202);
    let a = root_pixmap(&mut b, a_xid);
    let b_id = root_pixmap(&mut b, b_xid);
    assert_eq!(
        present_all(&mut b, 1, &[a_xid, b_xid, a_xid, b_xid, a_xid], STAGE),
        0
    );
    assert!(!is_promoted(&b, a) && !is_promoted(&b, b_id));
    assert_eq!(tokens(&b), 1, "a refused reservation spends nothing");

    clock.set(SERVER_SCANOUT_PROMOTE_REFILL * 3);
    observe(&mut b, b_xid, STAGE);
    assert!(is_promoted(&b, a) && is_promoted(&b, b_id));
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Complete));
}

#[test]
fn live_cap_holds_four_promoted_drawables() {
    let (mut b, _) = backend();
    let mut promoted = Vec::new();
    for n in 0..5u32 {
        let stage = 0x0060_0030 + n;
        add_stage(&mut b, stage);
        let source = 0x0065_0000 + n;
        promoted.push(root_pixmap(&mut b, source));
        observe(&mut b, source, stage);
        observe(&mut b, source, stage);
    }
    assert_eq!(b.pstate.server_scanout.live(), 4);
    assert!(!is_promoted(&b, promoted[4]));
    assert_eq!(b.pstate.scanout_m0.server.reject_cap_live, 1);

    b.store_decref_with_invalidate(promoted[0]);
    observe(&mut b, 0x0065_0004, 0x0060_0034);
    assert!(is_promoted(&b, promoted[4]));
    assert_eq!(b.pstate.server_scanout.live(), 4);
}

#[test]
fn promoted_source_reprobes_from_its_descriptor() {
    let (mut b, _) = backend();
    let a_xid = 0x0061_0061;
    let a = root_pixmap(&mut b, a_xid);
    observe(&mut b, a_xid, STAGE);
    observe(&mut b, a_xid, STAGE);
    let descriptor = b.scanout_m1_descriptor(a).expect("promoted descriptor");
    assert_eq!(descriptor.layout.pitch, u32::from(b.platform.fb_w) * 4);
    assert_eq!(descriptor.layout.modifier, 0);
}

// ── Task 7: export disqualification and the deferred DRI3 reply ──────────

thread_local! {
    /// Output disables seen by [`RecordingPower`].
    static DISABLED: Cell<usize> = const { Cell::new(0) };
    /// Each fake export: the drawable, whether a direct frame still
    /// referenced it, and the output disables seen before it ran.
    static EXPORTS: std::cell::RefCell<Vec<(DrawableId, bool, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Records the export it was asked for; the drawable must still be alive.
fn fake_exporter(
    b: &mut KmsBackend,
    id: DrawableId,
    _host_xid: u32,
) -> std::io::Result<yserver_core::backend::Dri3PixmapExport> {
    let drawable = b.store.get(id).expect("the exported storage is pinned");
    let (width, height) = (
        drawable.storage.extent.width,
        drawable.storage.extent.height,
    );
    let referenced = b.direct_frame_references_drawable(id);
    EXPORTS.with_borrow_mut(|exports| exports.push((id, referenced, DISABLED.get())));
    Ok(yserver_core::backend::Dri3PixmapExport {
        size: width * height * 4,
        width: u16::try_from(width).expect("test width"),
        height: u16::try_from(height).expect("test height"),
        stride: width * 4,
        offset: 0,
        depth: 24,
        bpp: 32,
        modifier: 0,
        fd: std::fs::File::open("/dev/null")
            .expect("open /dev/null")
            .into(),
    })
}

struct RecordingPower {
    fail: bool,
}

impl crate::kms::render::platform::power::OutputPowerExecutor for RecordingPower {
    fn disable(
        &self,
        _device: &crate::drm::Device,
        _output: &crate::platform::drm::Output,
    ) -> std::io::Result<()> {
        if self.fail {
            return Err(std::io::Error::other("scripted all-off failure"));
        }
        DISABLED.set(DISABLED.get() + 1);
        Ok(())
    }

    fn enable(
        &self,
        _device: &crate::drm::Device,
        _output: &crate::platform::drm::Output,
        _framebuffer: ::drm::control::framebuffer::Handle,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn disable_device(&self, _device: &crate::drm::Device) -> std::io::Result<()> {
        Ok(())
    }
}

/// The Task 6 fixture with the fake exporter, and source `xid` promoted.
fn promoted_backend(xid: u32) -> (KmsBackend, Rc<Cell<std::time::Duration>>, DrawableId) {
    DISABLED.set(0);
    EXPORTS.with_borrow_mut(Vec::clear);
    let (mut b, clock) = backend();
    b.pstate.dri3_drawable_exporter = fake_exporter;
    let id = root_pixmap(&mut b, xid);
    observe(&mut b, xid, STAGE);
    observe(&mut b, xid, STAGE);
    assert!(is_promoted(&b, id));
    (b, clock, id)
}

/// A live direct frame of `xid`, as the M2 flip leaves it in `current`.
fn scan_out(b: &mut KmsBackend, xid: u32) {
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    retain_direct_frame_from_source_test(b, xid, STAGE, w, h);
}

fn begin_export(b: &mut KmsBackend, xid: u32) -> yserver_core::backend::Dri3ExportBegin {
    Backend::dri3_begin_export_pixmap_buffers(b, xid).expect("begin the DRI3 export")
}

fn pending_token(
    begin: yserver_core::backend::Dri3ExportBegin,
) -> yserver_core::backend::Dri3ExportToken {
    match begin {
        yserver_core::backend::Dri3ExportBegin::Pending(token) => token,
        yserver_core::backend::Dri3ExportBegin::Ready(_) => {
            panic!("the export of a scanned-out source must wait")
        }
    }
}

/// The composed replacement of the direct frame is committed on `outputs`.
fn await_composed_replacement(b: &mut KmsBackend, outputs: &[usize]) {
    let root = b.pstate.direct.root_mut();
    for &output in outputs {
        root.unflip_outputs_to_replace.insert(output);
        root.unflip_awaiting_outputs
            .insert(output, 500 + u64::try_from(output).expect("small index"));
    }
}

/// The composed replacement retires on `output`.
fn retire_composed_replacement(b: &mut KmsBackend, output: usize) {
    let event = 500 + u64::try_from(output).expect("small index");
    let clock = yserver_core::backend::PresentClockSample {
        msc: 4,
        ust: 400,
        source: yserver_core::backend::PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(output, Some(event), clock));
}

fn disqualified_holding_descriptor(b: &KmsBackend, id: DrawableId) -> Option<bool> {
    match b.pstate.server_scanout.sources.get(&id) {
        Some(ServerScanoutState::Disqualified(descriptor)) => Some(descriptor.is_some()),
        _ => None,
    }
}

#[test]
fn dri3_export_of_the_live_direct_source_waits_for_its_replacement() {
    let a_xid = 0x0062_0001;
    let (mut b, _, a) = promoted_backend(a_xid);
    scan_out(&mut b, a_xid);

    let token = pending_token(begin_export(&mut b, a_xid));
    assert!(b.pstate.direct.root().unflip_requested);
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("server_source_exported")
    );
    assert_eq!(b.pstate.scanout_m0.server.export_deferred, 1);
    assert_eq!(b.pstate.scanout_m0.server.reject_exported, 1);
    assert_eq!(disqualified_holding_descriptor(&b, a), Some(true));
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a)),
        "the framebuffer stays while the CRTC scans it"
    );
    assert!(Backend::drain_ready_dri3_exports(&mut b).is_empty());
    assert!(
        Backend::next_wakeup(&b).is_some(),
        "the deadline wakes the loop"
    );

    await_composed_replacement(&mut b, &[0]);
    retire_composed_replacement(&mut b, 0);
    assert_eq!(Backend::drain_ready_dri3_exports(&mut b), [token]);
    assert!(Backend::drain_ready_dri3_exports(&mut b).is_empty());
    let export = Backend::finish_dri3_export(&mut b, token).expect("the deferred export");
    assert_eq!(u32::from(export.width), u32::from(b.platform.fb_w));
    assert_eq!(EXPORTS.with_borrow(Clone::clone), [(a, false, 0)]);
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert_eq!(disqualified_holding_descriptor(&b, a), Some(false));

    let pins = b.present_source_pins.len();
    Backend::release_dri3_export(&mut b, token);
    assert_eq!(b.present_source_pins.len(), pins - 1);
    assert!(b.pstate.server_scanout_exports.parked.is_empty());
}

#[test]
fn dri3_export_waits_for_every_crtc() {
    let a_xid = 0x0062_0011;
    let (mut b, _, _) = promoted_backend(a_xid);
    push_test_output(&mut b, 0x51);
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    scan_out(&mut b, a_xid);

    let token = pending_token(begin_export(&mut b, a_xid));
    await_composed_replacement(&mut b, &[0, 1]);
    retire_composed_replacement(&mut b, 0);
    assert!(
        Backend::drain_ready_dri3_exports(&mut b).is_empty(),
        "the second CRTC still scans the source"
    );
    retire_composed_replacement(&mut b, 1);
    assert_eq!(Backend::drain_ready_dri3_exports(&mut b), [token]);
}

#[test]
fn an_exported_source_never_flips_again() {
    let a_xid = 0x0062_0021;
    let (mut b, _, a) = promoted_backend(a_xid);
    // A pending flip of A with a successor of A queued behind it.
    scan_out(&mut b, a_xid);
    let mut pending = b.pstate.direct.root_mut().current.take().expect("A");
    pending.awaiting_outputs = std::iter::once(0).collect();
    b.pstate.direct.root_mut().pending = Some(pending);
    scan_out(&mut b, a_xid);
    let successor = b.pstate.direct.root_mut().current.take().expect("A again");
    b.queue_direct_successor(DirectScope::Root, successor);

    let token = pending_token(begin_export(&mut b, a_xid));
    let clock = yserver_core::backend::PresentClockSample {
        msc: 4,
        ust: 400,
        source: yserver_core::backend::PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, clock));
    assert!(
        b.pstate.direct.root().pending.is_none(),
        "the successor queued before the export must not flip"
    );
    await_composed_replacement(&mut b, &[0]);
    retire_composed_replacement(&mut b, 0);
    assert_eq!(Backend::drain_ready_dri3_exports(&mut b), [token]);
    Backend::finish_dri3_export(&mut b, token).expect("export");
    Backend::release_dri3_export(&mut b, token);

    let candidates = b.pstate.scanout_m0.server.candidates;
    for id in 40..60 {
        assert!(
            !present(&mut b, id, a_xid, STAGE),
            "Present {id} takes Copy"
        );
    }
    assert!(!b.pstate.direct.root().active());
    assert_eq!(b.pstate.scanout_m0.server.candidates, candidates);
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
}

#[test]
fn glx_bind_and_acquire_disqualify_without_parking() {
    let (a_xid, b_xid) = (0x0062_0031, 0x0062_0032);
    let (mut b, _, a) = promoted_backend(a_xid);
    let b_id = root_pixmap(&mut b, b_xid);
    let stage2 = 0x0060_0003;
    add_stage(&mut b, stage2);
    observe(&mut b, b_xid, stage2);
    observe(&mut b, b_xid, stage2);
    assert!(is_promoted(&b, b_id));
    scan_out(&mut b, a_xid);

    b.promote_pixmap_exportable(a_xid);
    assert_eq!(disqualified_holding_descriptor(&b, a), Some(true));
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("server_source_exported")
    );
    assert!(accepted_framebuffer(&b, a).is_some(), "still scanned out");

    b.acquire_glx_pixmap_export(b_xid);
    assert_eq!(
        disqualified_holding_descriptor(&b, b_id),
        Some(false),
        "an unreferenced source drops its framebuffer and descriptor at once"
    );
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(b_id, DirectScope::Root))
    );
    assert!(b.pstate.server_scanout_exports.parked.is_empty());
    assert_eq!(b.pstate.scanout_m0.server.reject_exported, 2);
    assert_eq!(
        rotation_state(&b, STAGE),
        Some(RotationState::Failed),
        "a disqualified member's rotation leaves Complete"
    );
}

#[test]
fn releasing_a_parked_export_releases_its_pin() {
    let a_xid = 0x0062_0041;
    let (mut b, _, a) = promoted_backend(a_xid);
    scan_out(&mut b, a_xid);
    let refcount = |b: &KmsBackend| b.store.get(a).map(|d| d.refcount);
    let before = refcount(&b);
    let token = pending_token(begin_export(&mut b, a_xid));
    assert_eq!(refcount(&b), before.map(|count| count + 1));
    // The client went away: the core releases without finishing.
    Backend::release_dri3_export(&mut b, token);
    assert_eq!(refcount(&b), before);
    assert!(b.pstate.server_scanout_exports.parked.is_empty());
    await_composed_replacement(&mut b, &[0]);
    retire_composed_replacement(&mut b, 0);
    assert!(Backend::drain_ready_dri3_exports(&mut b).is_empty());
    assert!(EXPORTS.with_borrow(Vec::is_empty));
}

#[test]
fn a_parked_export_survives_another_client_freeing_the_pixmap() {
    let a_xid = 0x0062_0051;
    let (mut b, _, a) = promoted_backend(a_xid);
    scan_out(&mut b, a_xid);
    let token = pending_token(begin_export(&mut b, a_xid));

    let refcount = |b: &KmsBackend| b.store.get(a).map(|d| d.refcount);
    let before = refcount(&b).expect("A");
    Backend::free_pixmap(&mut b, None, a_xid).expect("FreePixmap");
    assert_eq!(
        refcount(&b),
        Some(before - 1),
        "the client's reference is gone"
    );
    await_composed_replacement(&mut b, &[0]);
    retire_composed_replacement(&mut b, 0);
    assert_eq!(refcount(&b), Some(1), "only the export pin");

    assert_eq!(Backend::drain_ready_dri3_exports(&mut b), [token]);
    Backend::finish_dri3_export(&mut b, token).expect("the pinned storage exports");
    assert_eq!(EXPORTS.with_borrow(Clone::clone), [(a, false, 0)]);
    assert!(b.store.get(a).is_some());
    Backend::release_dri3_export(&mut b, token);
    assert!(b.store.get(a).is_none(), "freed once the pin is released");
    assert!(!b.pstate.server_scanout.sources.contains_key(&a));
}

#[test]
fn the_export_deadline_tears_direct_scanout_down_first() {
    let a_xid = 0x0062_0061;
    let (mut b, clock, a) = promoted_backend(a_xid);
    b.platform
        .for_tests_set_output_power_executor(Rc::new(RecordingPower { fail: false }));
    scan_out(&mut b, a_xid);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    let token = pending_token(begin_export(&mut b, a_xid));
    let deadline = b.pstate.server_scanout_exports.parked[&token].deadline;
    assert_eq!(
        deadline - (b.pstate.monotonic_clock)(),
        crate::kms::render::backend::SERVER_SCANOUT_EXPORT_WAIT
    );
    assert!(Backend::next_wakeup(&b).is_some_and(|wake| wake <= deadline));

    let mut state = yserver_core::server::ServerState::new();
    clock.set(crate::kms::render::backend::SERVER_SCANOUT_EXPORT_WAIT / 2);
    Backend::poll_deferred_input(&mut b, &mut state);
    assert!(Backend::drain_ready_dri3_exports(&mut b).is_empty());
    assert_eq!(DISABLED.get(), 0);

    clock.set(crate::kms::render::backend::SERVER_SCANOUT_EXPORT_WAIT);
    Backend::poll_deferred_input(&mut b, &mut state);
    assert_eq!(b.pstate.scanout_m0.server.export_deadline_teardown, 1);
    assert!(!b.pstate.direct.root().active());
    assert_eq!(Backend::drain_ready_dri3_exports(&mut b), [token]);
    Backend::finish_dri3_export(&mut b, token).expect("one reply with the fd");
    let outputs = b.platform.outputs.len();
    assert_eq!(
        EXPORTS.with_borrow(Clone::clone),
        [(a, false, outputs)],
        "every plane was off before the export ran"
    );
    assert!(Backend::drain_ready_dri3_exports(&mut b).is_empty());
}

#[test]
fn a_failed_deadline_teardown_fails_the_export_and_keeps_the_frame() {
    let a_xid = 0x0062_0071;
    let (mut b, clock, a) = promoted_backend(a_xid);
    b.platform
        .for_tests_set_output_power_executor(Rc::new(RecordingPower { fail: true }));
    scan_out(&mut b, a_xid);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    let frame_pins = {
        let frame = b.pstate.direct.root().current.as_ref().expect("frame");
        (frame.source_pin, frame.fallback_target_pin)
    };
    let token = pending_token(begin_export(&mut b, a_xid));
    let token_pin = b.pstate.server_scanout_exports.parked[&token].pin;

    clock.set(crate::kms::render::backend::SERVER_SCANOUT_EXPORT_WAIT);
    let mut state = yserver_core::server::ServerState::new();
    Backend::poll_deferred_input(&mut b, &mut state);
    assert_eq!(b.pstate.scanout_m0.server.export_deadline_fail, 1);
    assert_eq!(Backend::drain_ready_dri3_exports(&mut b), [token]);
    assert!(Backend::finish_dri3_export(&mut b, token).is_err());
    assert!(
        EXPORTS.with_borrow(Vec::is_empty),
        "no fd while a plane may scan it"
    );

    Backend::release_dri3_export(&mut b, token);
    assert!(!b.present_source_pins.contains_key(&token_pin));
    assert!(b.present_source_pins.contains_key(&frame_pins.0));
    assert!(b.present_source_pins.contains_key(&frame_pins.1));
    assert!(b.pstate.direct.root().current.is_some());
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a))
    );
    assert!(
        Backend::next_wakeup(&b).is_none_or(|wake| wake > (b.pstate.monotonic_clock)()),
        "a failed export does not keep the loop spinning"
    );
}

// ── Task 8: server-write fence on direct plane commits ───────────────────

fn recording_sink(
    b: &mut KmsBackend,
) -> Rc<crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor> {
    let recorder =
        Rc::new(crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor::default());
    b.platform.devices[0].atomic_commit_sink =
        Rc::new(crate::drm::atomic::AtomicCommitSink::new(recorder.clone()));
    recorder
}

const IN_FENCE_PROP: u32 = 0x7701;

fn in_fence_values(
    record: &crate::drm::atomic::tests_support::AtomicCommitRecord,
) -> Vec<(u32, u64)> {
    record
        .properties
        .iter()
        .filter(|update| u32::from(update.property) == IN_FENCE_PROP)
        .map(|update| (u32::from(update.object), update.value))
        .collect()
}

fn dev_null_fd() -> std::os::fd::OwnedFd {
    std::fs::File::open("/dev/null")
        .expect("open /dev/null")
        .into()
}

fn raw_value(fd: &std::os::fd::OwnedFd) -> u64 {
    i64::from(std::os::fd::AsRawFd::as_raw_fd(fd)) as u64
}

#[test]
fn a_fenced_direct_commit_sets_in_fence_on_every_plane() {
    use std::os::fd::AsFd as _;

    let mut b = KmsBackend::for_tests();
    let recorder = recording_sink(&mut b);
    push_test_output(&mut b, 0x51);
    for output in &mut b.platform.outputs {
        output.output.plane_in_fence_fd_prop =
            Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    }
    let fence = dev_null_fd();
    for in_fence in [Some(fence.as_fd()), None] {
        let platform = &b.platform;
        let planes: Vec<_> = platform
            .outputs
            .iter()
            .map(|layout| crate::drm::modeset::DirectScanoutPlaneState {
                output: &layout.output,
                src_x: 0,
                src_y: 0,
                src_w: u32::from(layout.width),
                src_h: u32::from(layout.height),
                in_fence_fd: in_fence,
            })
            .collect();
        crate::drm::modeset::submit_direct_scanout(
            &platform.devices[0].device,
            &platform.devices[0].atomic_commit_sink,
            ::drm::control::from_u32(91).expect("fb"),
            &planes,
            &[],
            false,
            crate::drm::async_flip::FlipTiming::Vsync,
            crate::drm::atomic::TearingCommitContext::NONE,
        )
        .expect("record the direct commit");
    }
    let records = recorder.records();
    let planes: Vec<u32> = b
        .platform
        .outputs
        .iter()
        .map(|layout| u32::from(layout.output.plane))
        .collect();
    let fenced: Vec<_> = planes
        .iter()
        .map(|plane| (*plane, raw_value(&fence)))
        .collect();
    assert_eq!(in_fence_values(&records[0]), fenced, "one fence per plane");
    assert!(
        in_fence_values(&records[1]).is_empty(),
        "no fence, no property"
    );
}

/// An imported source with an accepted probe entry, and a pending direct
/// frame of it awaiting output 0; returns the source xid.
fn pending_imported_frame(b: &mut KmsBackend) -> u32 {
    let (fallback_xid, source_xid) = (0x0063_0000, 0x0063_0001);
    seed_window(b, fallback_xid, None, 0, 0);
    let mut storage = Storage::for_tests_null(
        ash::vk::Extent2D {
            width: 100,
            height: 100,
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    storage.imported_dmabuf = Some(imported_dmabuf_for_test(100, 100));
    let source = b
        .store
        .allocate(source_xid, DrawableKind::Pixmap, 24, false, storage)
        .expect("imported source");
    b.pstate.scanout_m1.entries.insert(
        (source, DirectScope::Root),
        ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(fake_framebuffer_handle(
                source,
            )),
        ),
    );
    retain_direct_frame_from_source_test(b, source_xid, fallback_xid, 100, 100);
    let mut pending = b.pstate.direct.root_mut().current.take().expect("frame");
    pending.awaiting_outputs = std::iter::once(0).collect();
    b.pstate.direct.root_mut().pending = Some(pending);
    source_xid
}

#[test]
fn a_queued_successor_commits_the_fence_of_its_own_present() {
    let mut b = KmsBackend::for_tests();
    let recorder = recording_sink(&mut b);
    b.platform.outputs[0].output.plane_in_fence_fd_prop =
        Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    let source_xid = pending_imported_frame(&mut b);
    let fallback_xid = 0x0063_0000;
    retain_direct_frame_from_source_test(&mut b, source_xid, fallback_xid, 100, 100);
    let mut successor = b
        .pstate
        .direct
        .root_mut()
        .current
        .take()
        .expect("successor");
    let own = dev_null_fd();
    let own_raw = raw_value(&own);
    successor.in_fence = Some(own);
    b.queue_direct_successor(DirectScope::Root, successor);
    // A later write leaves a newer fence; the queued Present keeps its own.
    let later = dev_null_fd();
    assert_ne!(raw_value(&later), own_raw);

    let clock = yserver_core::backend::PresentClockSample {
        msc: 4,
        ust: 400,
        source: yserver_core::backend::PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, clock));
    assert!(
        b.pstate.direct.root().pending.is_some(),
        "the successor flipped"
    );
    let records = recorder.records();
    let plane = u32::from(b.platform.outputs[0].output.plane);
    assert_eq!(
        in_fence_values(records.last().expect("commit")),
        [(plane, own_raw)]
    );
}

#[test]
fn an_imported_direct_commit_carries_no_fence() {
    let mut b = KmsBackend::for_tests();
    let recorder = recording_sink(&mut b);
    b.platform.outputs[0].output.plane_in_fence_fd_prop =
        Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    let source_xid = pending_imported_frame(&mut b);
    retain_direct_frame_from_source_test(&mut b, source_xid, 0x0063_0000, 100, 100);
    let successor = b
        .pstate
        .direct
        .root_mut()
        .current
        .take()
        .expect("successor");
    b.queue_direct_successor(DirectScope::Root, successor);
    let clock = yserver_core::backend::PresentClockSample {
        msc: 4,
        ust: 400,
        source: yserver_core::backend::PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, clock));
    assert!(b.pstate.direct.root().pending.is_some());
    assert!(in_fence_values(recorder.records().last().expect("commit")).is_empty());
}

#[test]
fn the_direct_source_fence_follows_the_write_state() {
    use crate::kms::render::{
        backend::{ServerFenceKind, ServerFenceTally},
        store::ScanoutWriteFence,
    };

    let a_xid = 0x0063_0101;
    let (mut b, _, a) = promoted_backend(a_xid);
    assert!(b.store.is_scanout_fenced(a), "promotion fences the writes");
    let fence = |b: &mut KmsBackend| {
        let (fd, tally) = b
            .server_direct_source_fence(a, DirectScope::Root)
            .expect("fence");
        (fd.is_some(), tally)
    };
    let tally = |flush_added, kind| ServerFenceTally { flush_added, kind };

    // No write since promotion: no fence and no flush.
    assert_eq!(fence(&mut b), (false, tally(false, ServerFenceKind::None)));

    // An unsubmitted write is flushed first.
    b.store
        .touch_render_write(a, crate::kms::render::gpu::FenceTicket::for_tests_stub());
    assert!(b.store.has_unsubmitted_scanout_write(a));
    assert_eq!(fence(&mut b), (false, tally(true, ServerFenceKind::None)));
    b.store.untrack_scanout_writes(a);
    b.store.track_scanout_writes(a);

    // A sync_file, but a plane without IN_FENCE_FD: wait on the CPU.
    b.store.for_tests_set_scanout_write_fence(
        a,
        ScanoutWriteFence::Fence(std::sync::Arc::new(dev_null_fd())),
    );
    assert_eq!(
        fence(&mut b),
        (false, tally(false, ServerFenceKind::CpuWait))
    );

    // Every plane takes IN_FENCE_FD: the commit carries a dup.
    b.platform.outputs[0].output.plane_in_fence_fd_prop =
        Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    assert_eq!(
        fence(&mut b),
        (true, tally(false, ServerFenceKind::Explicit))
    );

    // The writing submit exported no sync_file: wait on the CPU.
    b.store
        .for_tests_set_scanout_write_fence(a, ScanoutWriteFence::ExportFailed);
    assert_eq!(
        fence(&mut b),
        (false, tally(false, ServerFenceKind::CpuWait))
    );
    assert!(b.pstate.server_scanout.fence_cpu_wait_warned);

    let server = &b.pstate.scanout_m0.server;
    assert_eq!(
        (
            server.direct_flush_added,
            server.direct_flush_reused,
            server.fence_explicit,
            server.fence_none,
            server.fence_cpu_wait,
        ),
        (0, 0, 0, 0, 0),
        "nothing was committed"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn an_unsubmitted_write_is_flushed_before_the_direct_commit_fence() {
    use crate::kms::render::{
        backend::{ServerFenceKind, ServerFenceTally},
        store::ScanoutWriteFence,
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    if !b.platform.gpu.vk().is_some_and(|vk| vk.supports_sync_fd()) {
        eprintln!("skipping: this Vulkan ICD exports no SYNC_FD");
        return;
    }
    for output in &mut b.platform.outputs {
        output.output.plane_in_fence_fd_prop =
            Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    }
    let pixmap = Backend::create_pixmap(&mut b, None, 24, 8, 8).expect("pixmap");
    let id = b.store.lookup(pixmap.as_raw()).expect("store entry");
    b.store.track_scanout_writes(id);
    Backend::put_image(&mut b, None, pixmap.as_raw(), 24, 1, 1, 0, 0, &[1, 2, 3, 0])
        .expect("write the source");
    assert!(b.store.has_unsubmitted_scanout_write(id));

    let (fence, tally) = b
        .server_direct_source_fence(id, DirectScope::Root)
        .expect("fence");
    assert!(fence.is_some(), "the flushed write's sync_file");
    assert!(
        !b.store.has_unsubmitted_scanout_write(id),
        "the write was submitted"
    );
    assert!(matches!(
        b.store.scanout_write_fence(id),
        Some(ScanoutWriteFence::Fence(_))
    ));
    assert_eq!(
        tally,
        ServerFenceTally {
            flush_added: true,
            kind: ServerFenceKind::Explicit,
        }
    );

    let (fence, tally) = b
        .server_direct_source_fence(id, DirectScope::Root)
        .expect("fence");
    assert!(fence.is_some());
    assert!(!tally.flush_added, "the write was already submitted");
}

// ── Task 9: direct flips of server-owned sources ──────────────────────────

thread_local! {
    /// Each direct frame submitted: its source and whether it had a fence.
    static SUBMITTED: std::cell::RefCell<Vec<(DrawableId, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The fake direct submitter, recording what it was handed.
fn recording_submitter(
    b: &mut KmsBackend,
    frame: &mut crate::kms::render::backend::DirectPresentFrame,
    scope: DirectScope,
    timing: crate::drm::async_flip::FlipTiming,
) -> std::io::Result<crate::drm::async_flip::AsyncOutcome> {
    SUBMITTED.with_borrow_mut(|submitted| {
        submitted.push((frame.source_id, frame.in_fence.is_some()));
    });
    KmsBackend::submit_direct_frame_for_tests(b, frame, scope, timing)
}

fn submitted() -> Vec<(DrawableId, bool)> {
    SUBMITTED.with_borrow(Clone::clone)
}

/// Marco's double buffer A/B promoted on `STAGE` and 7 of the 8 probation
/// Presents spent: the next eligible Present flips.
fn flipping_double_buffer(a_xid: u32, b_xid: u32) -> (KmsBackend, DrawableId, DrawableId) {
    SUBMITTED.with_borrow_mut(Vec::clear);
    let (mut b, _) = backend();
    b.pstate.direct_frame_submitter = recording_submitter;
    let a = root_pixmap(&mut b, a_xid);
    let b_id = root_pixmap(&mut b, b_xid);
    for xid in [a_xid, b_xid, a_xid, b_xid] {
        observe(&mut b, xid, STAGE);
    }
    assert!(is_promoted(&b, a) && is_promoted(&b, b_id));
    // No scene to sync cursor planes with: treat them as bound.
    b.pstate.direct.root_mut().cursor_bound_all = true;
    let probation = [b_xid, a_xid, b_xid, a_xid, b_xid, a_xid, b_xid];
    assert_eq!(present_all(&mut b, 1, &probation, STAGE), 0);
    assert!(submitted().is_empty());
    (b, a, b_id)
}

fn completions(b: &KmsBackend) -> Vec<(u64, u32)> {
    b.pstate
        .direct
        .root()
        .completed
        .iter()
        .map(|event| (event.present_id, u32::from(event.completion_mode)))
        .collect()
}

const FLIP: u32 = yserver_protocol::x11::present::COMPLETE_MODE_FLIP as u32;
const SKIP: u32 = yserver_protocol::x11::present::COMPLETE_MODE_SKIP as u32;

#[test]
fn server_owned_double_buffer_flips_and_idles_after_the_next_flip() {
    let (a_xid, b_xid) = (0x0064_0001, 0x0064_0002);
    let (mut b, a, b_id) = flipping_double_buffer(a_xid, b_xid);

    assert!(
        present(&mut b, 10, a_xid, STAGE),
        "the 8th eligible Present"
    );
    retire_flip(&mut b);
    assert!(b.pstate.direct.root().idled.is_empty(), "A is on screen");
    assert!(present(&mut b, 11, b_xid, STAGE));
    assert!(
        b.pstate.direct.root().idled.is_empty(),
        "A stays busy until B's flip retires"
    );
    retire_flip(&mut b);
    let idled: Vec<_> = b
        .pstate
        .direct
        .root()
        .idled
        .iter()
        .map(|event| event.present_id)
        .collect();
    assert_eq!(idled, [10]);
    assert!(present(&mut b, 12, a_xid, STAGE));
    retire_flip(&mut b);

    assert_eq!(completions(&b), [(10, FLIP), (11, FLIP), (12, FLIP)]);
    assert_eq!(
        submitted(),
        [(a, false), (b_id, false), (a, false)],
        "every Present scans a promoted buffer out, none is composed"
    );
    assert!(b.pstate.scanout_m0.server.unflip_reasons.is_empty());
    let server = &b.pstate.scanout_m0.server;
    assert_eq!(
        (
            server.fence_none,
            server.fence_explicit,
            server.fence_cpu_wait
        ),
        (3, 0, 0),
        "nothing wrote the sources since promotion"
    );
}

#[test]
fn a_server_owned_flip_carries_the_fence_of_its_writes() {
    use crate::kms::render::store::ScanoutWriteFence;

    let (a_xid, b_xid) = (0x0064_0011, 0x0064_0012);
    let (mut b, a, _) = flipping_double_buffer(a_xid, b_xid);
    let recorder = recording_sink(&mut b);
    b.pstate.direct_frame_submitter = KmsBackend::submit_direct_frame_live;
    for output in &mut b.platform.outputs {
        output.output.plane_in_fence_fd_prop =
            Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    }
    let fence = std::sync::Arc::new(dev_null_fd());
    b.store
        .for_tests_set_scanout_write_fence(a, ScanoutWriteFence::Fence(fence));
    assert!(present(&mut b, 10, a_xid, STAGE));
    let records = recorder.records();
    let record = records.last().expect("the direct commit");
    let output = &b.platform.outputs[0].output;
    let fences = in_fence_values(record);
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].0, u32::from(output.plane));
    assert_eq!(b.pstate.scanout_m0.server.fence_explicit, 1);
}

#[test]
fn a_present_during_a_pending_flip_queues_as_latest_wins_successor() {
    use crate::kms::render::store::ScanoutWriteFence;

    let (a_xid, b_xid) = (0x0064_0021, 0x0064_0022);
    let (mut b, a, b_id) = flipping_double_buffer(a_xid, b_xid);
    for output in &mut b.platform.outputs {
        output.output.plane_in_fence_fd_prop =
            Some(::drm::control::from_u32(IN_FENCE_PROP).expect("handle"));
    }
    assert!(present(&mut b, 10, a_xid, STAGE));
    assert!(present(&mut b, 11, b_xid, STAGE));
    // B is written after Present 11 was queued: Present 12 carries the fence.
    b.store.for_tests_set_scanout_write_fence(
        b_id,
        ScanoutWriteFence::Fence(std::sync::Arc::new(dev_null_fd())),
    );
    assert!(present(&mut b, 12, b_xid, STAGE));
    assert_eq!(submitted().len(), 1, "both wait behind the flip");
    let queued = b.pstate.direct.root().queued_successor.as_ref();
    assert_eq!(queued.map(|frame| frame.event.present_id), Some(12));
    retire_flip(&mut b);
    assert_eq!(completions(&b), [(10, FLIP), (11, SKIP)]);
    assert_eq!(submitted(), [(a, false), (b_id, true)]);
    let server = &b.pstate.scanout_m0.server;
    assert_eq!(
        (server.fence_none, server.fence_explicit),
        (1, 1),
        "the skipped Present 11 counts no fence"
    );
}

#[test]
fn a_third_source_unflips_and_its_promoted_members_take_copy() {
    let (a_xid, b_xid, c_xid) = (0x0064_0031, 0x0064_0032, 0x0064_0033);
    let (mut b, _, _) = flipping_double_buffer(a_xid, b_xid);
    root_pixmap(&mut b, c_xid);
    assert!(present(&mut b, 10, a_xid, STAGE));
    retire_flip(&mut b);
    assert!(!present(&mut b, 11, c_xid, STAGE));
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("server_source_rotation_changed")
    );
    assert_eq!(rotation_state(&b, STAGE), Some(RotationState::Observing));
    assert!(
        !present(&mut b, 12, b_xid, STAGE),
        "B is promoted, not Complete"
    );
    assert_eq!(submitted().len(), 1);
}

#[test]
fn a_promoted_source_is_no_probe_reject_and_reprobes_after_a_clear() {
    let a_xid = 0x0064_0041;
    let (mut b, _) = backend();
    let a = root_pixmap(&mut b, a_xid);
    observe(&mut b, a_xid, STAGE);
    let before = b.pstate.scanout_m0.reject_server_owned;
    assert_eq!(before, 1, "a candidate without a descriptor");
    let gate_before = b.pstate.scanout_m0.m1_gate_reject_import;
    observe(&mut b, a_xid, STAGE);
    assert!(is_promoted(&b, a));
    observe(&mut b, a_xid, STAGE);
    assert_eq!(b.pstate.scanout_m0.reject_server_owned, before);

    b.pstate.scanout_m1.clear("test topology change");
    observe(&mut b, a_xid, STAGE);
    assert!(
        b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root)),
        "re-probed from the descriptor"
    );
    assert_eq!(b.pstate.scanout_m0.m1_gate_reject_import, gate_before);
    assert_eq!(b.pstate.scanout_m0.reject_server_owned, before);
    assert_eq!(attempts(&b), 1, "never promoted again");
}

// ── Task 10: invalidation and framebuffer lifetime ───────────────────────

thread_local! {
    /// Each fake M1 import: the framebuffer handle it returned.
    static IMPORTS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Imports every probe source as a synthetic framebuffer, so the M1 probe
/// reaches `TEST_ONLY` on the recording sink.
fn fake_importer(
    _device: Rc<crate::drm::Device>,
    _fd: std::os::fd::BorrowedFd<'_>,
    _layout: &ScanoutSourceLayout,
) -> std::io::Result<crate::drm::modeset::DirectScanoutProbeFramebuffer> {
    let raw = IMPORTS.with_borrow_mut(|imports| {
        let raw = 0xA000 + u32::try_from(imports.len()).expect("few imports");
        imports.push(raw);
        raw
    });
    Ok(
        crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
            ::drm::control::from_u32(raw).expect("nonzero test handle"),
        ),
    )
}

fn imports() -> Vec<u32> {
    IMPORTS.with_borrow(Clone::clone)
}

/// [`promoted_backend`] with the fake importer and a recording sink.
fn reprobing_backend(
    xid: u32,
) -> (
    KmsBackend,
    DrawableId,
    Rc<crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor>,
) {
    IMPORTS.with_borrow_mut(Vec::clear);
    let (mut b, _, id) = promoted_backend(xid);
    b.pstate.scanout_m1_importer = fake_importer;
    let recorder = recording_sink(&mut b);
    (b, id, recorder)
}

/// The raw fd of `id`'s descriptor and whether it is still an open fd.
fn descriptor_fd(b: &KmsBackend, id: DrawableId) -> Option<(i32, bool)> {
    use std::os::fd::AsRawFd as _;
    let raw = b.pstate.server_scanout.descriptor(id)?.fd.as_raw_fd();
    // SAFETY: F_GETFD only queries the descriptor flags.
    let open = unsafe { libc::fcntl(raw, libc::F_GETFD) } != -1;
    Some((raw, open))
}

/// A same-size mode change: the topology signature moves, the root does not.
fn change_mode_clock(b: &mut KmsBackend) {
    b.platform.outputs[0].output.picked.clock_khz += 1;
}

/// The direct frame is replaced on output 0 and released.
fn unflip_and_retire(b: &mut KmsBackend) {
    b.pstate.request_direct_unflip("test_unflip");
    await_composed_replacement(b, &[0]);
    retire_composed_replacement(b, 0);
    assert!(!b.pstate.direct.root().active());
}

#[test]
fn promotion_replaces_a_rejected_probe_entry() {
    let (mut b, _) = backend();
    let a_xid = 0x0065_0001;
    let a = root_pixmap(&mut b, a_xid);
    b.pstate
        .scanout_m1
        .entries
        .insert((a, DirectScope::Root), ScanoutM1ProbeEntry::rejected());
    observe(&mut b, a_xid, STAGE);
    assert_eq!(accepted_framebuffer(&b, a), None);
    observe(&mut b, a_xid, STAGE);
    assert!(is_promoted(&b, a));
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a))
    );
}

#[test]
fn a_retired_source_drops_its_entry_descriptor_and_fence_state() {
    let a_xid = 0x0065_0011;
    let (mut b, _, a) = promoted_backend(a_xid);
    assert!(b.store.is_scanout_fenced(a));
    b.store_decref_with_invalidate(a);
    assert!(b.store.get(a).is_none());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert!(!b.pstate.server_scanout.sources.contains_key(&a));
    assert!(!b.store.is_scanout_fenced(a));
    assert!(b.store.scanout_write_fence(a).is_none());
}

#[test]
fn a_freed_source_keeps_its_framebuffer_while_scanned_out() {
    let a_xid = 0x0065_0021;
    let (mut b, _, a) = promoted_backend(a_xid);
    scan_out(&mut b, a_xid);
    let fd = descriptor_fd(&b, a).expect("descriptor");
    b.store_decref_with_invalidate(a);
    assert!(
        b.store.get(a).is_some(),
        "the direct frame pins the storage"
    );
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a))
    );
    assert_eq!(descriptor_fd(&b, a), Some(fd));

    unflip_and_retire(&mut b);
    assert!(b.store.get(a).is_none());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert!(!b.pstate.server_scanout.sources.contains_key(&a));
}

#[test]
fn a_topology_change_keeps_a_live_server_owned_framebuffer_until_its_crtc_is_replaced() {
    // Mutation: drop the active-owner check in sync_scanout_m1_topology (#311).
    let a_xid = 0x0065_0031;
    let (mut b, a, _) = reprobing_backend(a_xid);
    scan_out(&mut b, a_xid);
    let signature = b.pstate.scanout_m1.topology_signature;
    let fd = descriptor_fd(&b, a).expect("descriptor");
    assert!(fd.1);

    change_mode_clock(&mut b);
    observe(&mut b, a_xid, STAGE);
    assert_eq!(
        accepted_framebuffer(&b, a),
        Some(fake_framebuffer_handle(a)),
        "the CRTC still scans the framebuffer"
    );
    assert_eq!(b.pstate.scanout_m1.topology_signature, signature);
    assert_eq!(descriptor_fd(&b, a), Some(fd));
    assert!(imports().is_empty());

    unflip_and_retire(&mut b);
    assert!(b.sync_scanout_m1_topology());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root)),
        "the entry goes once no CRTC scans it"
    );
    assert!(is_promoted(&b, a), "the descriptor stays");
    assert_eq!(descriptor_fd(&b, a), Some(fd));
}

#[test]
fn after_a_topology_change_a_promoted_source_reprobes_from_its_descriptor() {
    let a_xid = 0x0065_0041;
    let (mut b, a, recorder) = reprobing_backend(a_xid);
    change_mode_clock(&mut b);
    observe(&mut b, a_xid, STAGE);
    assert_eq!(imports(), [0xA000]);
    assert_eq!(
        accepted_framebuffer(&b, a).map(u32::from),
        Some(0xA000),
        "re-probed from the descriptor"
    );
    let test_only = recorder
        .records()
        .iter()
        .filter(|record| {
            record
                .flags
                .contains(::drm::control::AtomicCommitFlags::TEST_ONLY)
        })
        .count();
    assert_eq!(test_only, 1);
    assert_eq!(attempts(&b), 1, "never promoted again");
    observe(&mut b, a_xid, STAGE);
    assert_eq!(imports().len(), 1, "the accepted entry is cached");
}

#[test]
fn a_crtc_config_change_drops_the_entry_after_the_crtcs_leave_it() {
    let a_xid = 0x0065_0051;
    let (mut b, _, a) = promoted_backend(a_xid);
    b.platform
        .for_tests_set_output_power_executor(Rc::new(RecordingPower { fail: false }));
    scan_out(&mut b, a_xid);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    let fd = descriptor_fd(&b, a).expect("descriptor");
    let name = b.platform.outputs[0].key.connector_name.clone();
    let id = Backend::randr_outputs_and_modes(&mut b)
        .0
        .into_iter()
        .find(|output| output.name == name)
        .expect("output in the RandR projection")
        .output_id;

    // The fd-less fixture cannot relight; only the teardown matters.
    let _ = Backend::apply_crtc_config(&mut b, id, &name, None, 0, 0);
    assert!(DISABLED.get() > 0, "the CRTCs were turned off");
    assert!(!b.pstate.direct.root().active());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert!(is_promoted(&b, a));
    assert_eq!(descriptor_fd(&b, a), Some(fd));
}

#[test]
fn vt_suspend_drops_the_entry_and_keeps_the_descriptor() {
    let a_xid = 0x0065_0061;
    let (mut b, _, a) = promoted_backend(a_xid);
    b.platform
        .for_tests_set_output_power_executor(Rc::new(RecordingPower { fail: false }));
    scan_out(&mut b, a_xid);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    let fd = descriptor_fd(&b, a).expect("descriptor");
    let mut state = yserver_core::server::ServerState::new();
    b.inject_seat_event_for_test(&mut state, false);
    assert!(DISABLED.get() > 0);
    assert!(!b.pstate.direct.root().active());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert!(is_promoted(&b, a));
    assert_eq!(descriptor_fd(&b, a), Some(fd));
}

#[test]
fn dpms_off_drops_the_entry_and_keeps_the_descriptor() {
    let a_xid = 0x0065_0071;
    let (mut b, _, a) = promoted_backend(a_xid);
    b.platform
        .for_tests_set_output_power_executor(Rc::new(RecordingPower { fail: false }));
    scan_out(&mut b, a_xid);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    let fd = descriptor_fd(&b, a).expect("descriptor");
    Backend::set_dpms_power(&mut b, 3).expect("DPMS off");
    assert!(DISABLED.get() > 0);
    assert!(!b.pstate.direct.root().active());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert!(is_promoted(&b, a));
    assert_eq!(descriptor_fd(&b, a), Some(fd));
}

#[test]
fn shutdown_drops_the_entry_before_the_descriptor() {
    let a_xid = 0x0065_0081;
    let (mut b, _, a) = promoted_backend(a_xid);
    b.platform
        .for_tests_set_output_power_executor(Rc::new(RecordingPower { fail: false }));
    scan_out(&mut b, a_xid);
    let fd = descriptor_fd(&b, a).expect("descriptor");
    b.disable_output().expect("shutdown");
    assert!(!b.pstate.direct.root().active());
    assert!(
        !b.pstate
            .scanout_m1
            .entries
            .contains_key(&(a, DirectScope::Root))
    );
    assert_eq!(
        descriptor_fd(&b, a),
        Some(fd),
        "the fd outlives the framebuffer"
    );
}

// ── Task 11: revalidation, fallbacks and the COW shadow ──────────────────

/// Marco's A/B with A on screen after a retired direct flip of Present 10.
fn live_server_owned_frame(a_xid: u32, b_xid: u32) -> (KmsBackend, DrawableId, DrawableId) {
    let (mut b, a, b_id) = flipping_double_buffer(a_xid, b_xid);
    assert!(present(&mut b, 10, a_xid, STAGE));
    retire_flip(&mut b);
    let current = b.pstate.direct.root().current.as_ref().expect("A is live");
    assert_eq!(current.source_id, a);
    assert_eq!(
        current.target,
        crate::kms::render::backend::ScanoutM0Target::CowDescendant,
        "marco's stage under the COW"
    );
    (b, a, b_id)
}

fn unflip_reason(b: &KmsBackend) -> Option<&'static str> {
    b.pstate.direct.root().unflip_reason
}

#[test]
fn a_cow_shape_change_unflips_a_server_owned_frame() {
    use yserver_protocol::x11::xfixes::RegionRect;

    let (a_xid, b_xid) = (0x0066_0001, 0x0066_0002);
    let (mut b, _, _) = live_server_owned_frame(a_xid, b_xid);
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    let half = RegionRect {
        x: 0,
        y: 0,
        width: w / 2,
        height: h,
    };
    Backend::set_shape_rectangles(&mut b, None, cow, 0, Some(&[half])).expect("shape");
    assert_eq!(unflip_reason(&b), Some("shape_clips_direct_frame"));
    assert!(!present(&mut b, 11, b_xid, STAGE), "Copy");
    assert_eq!(submitted().len(), 1);
}

// This fixture has no scene, so `cursor_mode()` is `Sw` and every composite
// tick of a live direct frame unflips on the cursor; these two triggers are
// proved at the Present gate, which reads the cursor and the root overlay.

#[test]
fn a_visible_software_cursor_sends_the_next_present_to_copy() {
    let (a_xid, b_xid) = (0x0066_0011, 0x0066_0012);
    let (mut b, _, _) = live_server_owned_frame(a_xid, b_xid);
    assert!(b.direct_cursor_eligible_for(&b.direct_scope_outputs(DirectScope::Root)));
    Backend::set_cursor_hidden(&mut b, false);
    assert!(
        !b.direct_cursor_eligible_for(&b.direct_scope_outputs(DirectScope::Root)),
        "the cursor left the plane"
    );
    assert!(!present(&mut b, 11, b_xid, STAGE), "Copy");
    assert_eq!(
        unflip_reason(&b),
        Some("ineligible_authoritative_root_present")
    );
    assert_eq!(submitted().len(), 1);
}

#[test]
fn a_root_overlay_sends_the_next_present_to_copy() {
    let (a_xid, b_xid) = (0x0066_0021, 0x0066_0022);
    let (mut b, _, _) = live_server_owned_frame(a_xid, b_xid);
    b.scene.root_overlay_toggle(
        yserver_protocol::x11::ClientId(7),
        0xff_ffff,
        &[ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 10, y: 10 },
            extent: ash::vk::Extent2D {
                width: 40,
                height: 20,
            },
        }],
    );
    assert!(!present(&mut b, 11, b_xid, STAGE), "Copy");
    assert_eq!(
        unflip_reason(&b),
        Some("ineligible_authoritative_root_present")
    );
    assert_eq!(submitted().len(), 1);
}

#[test]
fn a_restack_beneath_the_cow_keeps_a_server_owned_frame() {
    // The COW stays above every root child (`compGetWindowTopmostChild`),
    // so a mapped and raised panel reaches the screen through marco's frame.
    let (a_xid, b_xid) = (0x0066_0031, 0x0066_0032);
    let (mut b, _, _) = live_server_owned_frame(a_xid, b_xid);
    let panel = 0x0066_0039;
    seed_window(&mut b, panel, None, 0, 0);
    Backend::map_subwindow(&mut b, None, panel).expect("map the panel");
    b.core.top_level_order = vec![panel];
    Backend::sync_top_level_order(&mut b, &yserver_core::server::ServerState::new());
    assert_eq!(unflip_reason(&b), None);
    assert!(present(&mut b, 11, b_xid, STAGE), "still direct");
}

#[test]
fn unmapping_the_destination_unflips_a_server_owned_frame() {
    let (a_xid, b_xid) = (0x0066_0041, 0x0066_0042);
    let (mut b, _, _) = live_server_owned_frame(a_xid, b_xid);
    Backend::unmap_subwindow(&mut b, None, STAGE).expect("unmap");
    assert_eq!(unflip_reason(&b), Some("unmap_direct_frame_drawable"));
    assert!(!present(&mut b, 11, b_xid, STAGE), "Copy");
}

#[test]
fn releasing_the_overlay_window_unflips_a_server_owned_frame() {
    let (a_xid, b_xid) = (0x0066_0051, 0x0066_0052);
    let (mut b, _, _) = live_server_owned_frame(a_xid, b_xid);
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    b.cow_id = b.store.lookup(cow);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    assert!(Backend::prepare_overlay_release(&mut b, None).expect("prepare"));
    Backend::retire_overlay_window(&mut b, None);
    assert_eq!(unflip_reason(&b), Some("release_last_overlay_window"));
    assert!(b.cow_id.is_some(), "the COW outlives the direct frame");
    unflip_and_retire(&mut b);
    assert!(b.cow_id.is_none());
}

#[test]
fn a_rejected_reprobe_takes_copy_until_an_invalidation() {
    let a_xid = 0x0066_0061;
    let (mut b, a, recorder) = reprobing_backend(a_xid);
    let test_only =
        |recorder: &crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor| {
            recorder
                .records()
                .iter()
                .filter(|record| {
                    record
                        .flags
                        .contains(::drm::control::AtomicCommitFlags::TEST_ONLY)
                })
                .count()
        };
    change_mode_clock(&mut b);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    observe(&mut b, a_xid, STAGE);
    assert_eq!(test_only(&recorder), 1);
    let entry = b
        .pstate
        .scanout_m1
        .entries
        .get(&(a, DirectScope::Root))
        .expect("a negative entry");
    assert!(entry.framebuffer().is_none());
    assert!(is_promoted(&b, a), "the descriptor stays");

    for id in 20..30 {
        assert!(
            !present(&mut b, id, a_xid, STAGE),
            "Present {id} takes Copy"
        );
    }
    assert_eq!(
        test_only(&recorder),
        1,
        "no re-probe before an invalidation"
    );

    change_mode_clock(&mut b);
    observe(&mut b, a_xid, STAGE);
    assert_eq!(test_only(&recorder), 2, "re-probed after the invalidation");
    assert!(accepted_framebuffer(&b, a).is_some());
    assert_eq!(attempts(&b), 1, "never promoted again");
}

#[test]
fn a_failed_source_fence_takes_copy_and_unflips() {
    use crate::kms::render::store::ScanoutWriteFence;

    fn failing_waiter(_b: &KmsBackend, _id: DrawableId) -> std::io::Result<()> {
        Err(std::io::Error::other("scripted fence wait failure"))
    }

    let (a_xid, b_xid) = (0x0066_0071, 0x0066_0072);
    let (mut b, _, b_id) = live_server_owned_frame(a_xid, b_xid);
    b.pstate.direct_source_cpu_waiter = failing_waiter;
    b.store
        .for_tests_set_scanout_write_fence(b_id, ScanoutWriteFence::ExportFailed);
    assert!(!present(&mut b, 11, b_xid, STAGE), "Copy");
    assert_eq!(unflip_reason(&b), Some("server_source_fence_failed"));
    let root = b.pstate.direct.root();
    assert_eq!(root.unflip_fallback_source, Some(b_id));
    assert!(root.queued_successor.is_none() && root.pending.is_none());
    assert_eq!(submitted().len(), 1, "only Present 10 was committed");
    let server = &b.pstate.scanout_m0.server;
    assert_eq!(
        (server.fence_none, server.fence_cpu_wait),
        (1, 0),
        "the failed Present counts no fence"
    );
}

#[test]
fn a_present_that_falls_to_copy_after_its_fence_counts_nothing() {
    let (a_xid, b_xid) = (0x0066_0081, 0x0066_0082);
    let (mut b, _, _) = flipping_double_buffer(a_xid, b_xid);
    b.scene.test_set_flip_in_flight(true);
    assert!(
        !present(&mut b, 10, a_xid, STAGE),
        "a scene flip is pending"
    );
    let counts = |b: &KmsBackend| {
        let server = &b.pstate.scanout_m0.server;
        (
            server.direct_flush_added + server.direct_flush_reused,
            server.fence_none + server.fence_explicit + server.fence_cpu_wait,
        )
    };
    assert_eq!(counts(&b), (0, 0));
    b.scene.test_set_flip_in_flight(false);
    assert!(present(&mut b, 11, a_xid, STAGE));
    assert_eq!(counts(&b), (1, 1));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_unflip_shadow_of_a_promoted_source_holds_its_pixels() {
    let (w, h) = (32_u16, 16_u16);
    let mut b = match KmsBackend::for_tests_with_vk_dimensions(w, h) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let source = Backend::create_pixmap(&mut b, None, 24, w, h).expect("source");
    let fallback = Backend::create_pixmap(&mut b, None, 24, w, h).expect("fallback target");
    let source_id = b.store.lookup(source.as_raw()).expect("source entry");
    if let Err(error) = b.engine.promote_drawable_for_scanout(
        &mut b.platform.gpu,
        &mut b.store,
        source_id,
        &[],
        |_| Ok::<(), ()>(()),
    ) {
        eprintln!("skipping: this Vulkan ICD cannot promote for scanout ({error:?})");
        return;
    }
    assert!(
        b.store
            .get(source_id)
            .is_some_and(|drawable| drawable.storage.is_exportable()),
        "the source is promoted linear storage"
    );

    let mut written = Vec::with_capacity(usize::from(w) * usize::from(h) * 4);
    for y in 0..h {
        for x in 0..w {
            let (x, y) = (x as u8, y as u8);
            written.extend_from_slice(&[x.wrapping_mul(7), y.wrapping_mul(13), x ^ y, 0]);
        }
    }
    Backend::put_image(&mut b, None, source.as_raw(), 24, w, h, 0, 0, &written)
        .expect("write the source");
    retain_direct_frame_from_source_test(&mut b, source.as_raw(), fallback.as_raw(), w, h);
    b.materialize_direct_shadow_for_unflip(DirectScope::Root)
        .expect("materialize the unflip shadow");
    assert!(b.pstate.direct.root().unflip_shadow_ready);

    let fallback_id = b.store.lookup(fallback.as_raw()).expect("fallback entry");
    let shadow = b
        .engine
        .get_image(
            &mut b.store,
            &mut b.platform.gpu,
            crate::kms::render::target::Src::server_internal(fallback_id),
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
            },
            24,
        )
        .expect("read the shadow");
    // Depth 24: the fourth byte of each pixel is padding, not pixel data.
    let colour = |bytes: &[u8]| -> Vec<[u8; 3]> {
        bytes
            .chunks_exact(4)
            .map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect()
    };
    assert_eq!(colour(&shadow), colour(&written));
}

/// Move the frame `install_direct_frame_for_target_test` left on the Root
/// owner to the `Output(output_idx)` owner, as #310's per-output admission
/// would hold it.
fn hold_on_output_owner(b: &mut KmsBackend, output_idx: usize, source: Option<DrawableId>) {
    let fallback = b.store.lookup(STAGE).expect("the stage window");
    install_direct_frame_for_target_test(b, 0x0067_0001, fallback, true);
    let root = b.pstate.direct.root_mut();
    let mut frame = root.current.take().expect("the installed frame");
    root.hold_direct = false;
    if let Some(source) = source {
        frame.source_id = source;
    }
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    let owner = &mut b.pstate.direct.outputs[output_idx];
    owner.current = Some(frame);
    owner.hold_direct = true;
    assert_eq!(
        b.pstate.direct.active_scopes(),
        vec![DirectScope::Output(output_idx)]
    );
}

#[test]
fn disqualification_leaves_an_unrelated_output_owner_direct() {
    // Mutation: request_direct_unflip_referencing unflips every active owner.
    let a_xid = 0x0067_0011;
    let (mut b, _, _) = promoted_backend(a_xid);
    hold_on_output_owner(&mut b, 0, None);
    let begin = begin_export(&mut b, a_xid);
    assert!(
        matches!(begin, yserver_core::backend::Dri3ExportBegin::Ready(_)),
        "no owner shows the source: the export is not deferred"
    );
    let owner = &b.pstate.direct.outputs[0];
    assert!(
        !owner.unflip_requested,
        "the output owner shows another client"
    );
}

#[test]
fn an_output_owner_holding_a_promoted_source_defers_its_export_and_unflips() {
    // Mutation: direct_scopes_referencing_drawable reads only the Root owner.
    let a_xid = 0x0067_0021;
    let (mut b, _, a) = promoted_backend(a_xid);
    hold_on_output_owner(&mut b, 0, Some(a));
    assert!(b.direct_frame_references_drawable(a));
    let _token = pending_token(begin_export(&mut b, a_xid));
    let owner = &b.pstate.direct.outputs[0];
    assert!(
        owner.unflip_requested,
        "the owner showing the source unflips"
    );
    assert_eq!(owner.unflip_reason, Some("server_source_exported"));
    assert!(!b.pstate.direct.root().unflip_requested);
}
