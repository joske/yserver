//! Candidate tracking, rotations and caps for direct scanout of server-owned
//! Present sources (plan Task 6), driven through the M0 observation with a
//! fake promoter.

use super::{
    present_scanout::{bind_test_randr_crtc, seed_cow_stage},
    *,
};
use crate::kms::render::{
    backend::{
        RotationState, SERVER_SCANOUT_PROMOTE_BURST, SERVER_SCANOUT_PROMOTE_REFILL,
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
        .get(&id)
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

    // Promoted server-owned sources stay on Copy until their writes are
    // fenced (plan Tasks 7–9), past M2 probation too.
    let sources: Vec<u32> = [a_xid, b_xid].repeat(6);
    assert_eq!(present_all(&mut b, 10, &sources, STAGE), 0);
    assert!(!b.pstate.direct.root().active());
    assert_eq!(attempts(&b), 2);

    // Control: the same stream flips once the sources are not promoted.
    b.pstate.server_scanout.sources.clear();
    assert!(present(&mut b, 30, a_xid, STAGE));
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
    assert!(!b.pstate.scanout_m1.entries.contains_key(&b_id));

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
    // Stand-in for a live direct frame of A (server-owned flips are Task 9).
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
    assert!(!b.pstate.scanout_m1.entries.contains_key(&a));
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
