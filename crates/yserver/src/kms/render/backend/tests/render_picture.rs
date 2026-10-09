use super::*;

// ─── Stage 3b: picture record lifecycle tests ──────────────

/// `picture_record_lifecycle` per plan §3b: create → change →
/// free, with every value-mask bit exercised at least once.
/// Round-trip via `KmsCore.pictures.get` after each step.
#[test]
fn picture_record_lifecycle_exercises_every_value_mask_bit() {
    use crate::kms::core::PictureFilter;
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    // Pre-create a fake drawable xid so render_create_picture's
    // store.lookup doesn't have to be Some — the picture record
    // just stores the host_xid; the incref path is exercised
    // in the next test.
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));

    // CPRepeat=Pad, CPAlphaMap=0xDEAD_BEEF, CPAlphaXOrigin=10,
    // CPAlphaYOrigin=20, CPClipXOrigin=30, CPClipYOrigin=40,
    // CPClipMask=0 (= None), CPGraphicsExposure=1,
    // CPSubwindowMode=1, CPPolyEdge=1, CPPolyMode=1,
    // CPDither=1 (consumed-but-not-stored), CPComponentAlpha=1.
    let value_mask: u32 = 0x0001
        | 0x0002
        | 0x0004
        | 0x0008
        | 0x0010
        | 0x0020
        | 0x0040
        | 0x0080
        | 0x0100
        | 0x0200
        | 0x0400
        | 0x0800
        | 0x1000;
    let mut values: Vec<u8> = Vec::new();
    for v in [
        2_u32,       // Repeat::Pad
        0xDEAD_BEEF, // alpha_map
        10,          // alpha_x
        20,          // alpha_y
        30,          // clip_x
        40,          // clip_y
        0,           // clip_mask = None
        1,           // graphics_exposure
        1,           // subwindow_mode
        1,           // poly_edge
        1,           // poly_mode
        1,           // dither (consumed, not stored)
        1,           // component_alpha
    ] {
        values.extend_from_slice(&v.to_le_bytes());
    }

    let picture = b
        .render_create_picture(None, drawable, 0, value_mask, &values)
        .expect("create_picture")
        .expect("Some(handle)");
    let pic_xid = picture.as_raw();

    // Find and unpack the resulting record.
    let rec = b.core.pictures.get(&pic_xid).expect("record present");
    match rec {
        PictureRecord::Drawable {
            host_xid,
            pict_format: _,
            clip,
            clip_x,
            clip_y,
            repeat,
            alpha_map,
            alpha_x,
            alpha_y,
            component_alpha,
            transform,
            filter,
            graphics_exposure,
            subwindow_mode,
            poly_edge,
            poly_mode,
            drawable_origin: _,
        } => {
            assert_eq!(*host_xid, 0x4242_4242);
            assert!(clip.is_none(), "clip stays None for clip_mask=0");
            assert_eq!(*clip_x, 30);
            assert_eq!(*clip_y, 40);
            assert_eq!(*repeat, Repeat::Pad);
            assert_eq!(*alpha_map, Some(0xDEAD_BEEF));
            assert_eq!(*alpha_x, 10);
            assert_eq!(*alpha_y, 20);
            assert!(*component_alpha);
            assert!(transform.is_none());
            assert_eq!(*filter, PictureFilter::Nearest);
            assert!(*graphics_exposure);
            assert_eq!(*subwindow_mode, 1);
            assert_eq!(*poly_edge, 1);
            assert_eq!(*poly_mode, 1);
        }
        other => panic!("expected Drawable, got {other:?}"),
    }

    // ChangePicture override of a single bit (CPRepeat=Normal).
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&pic_xid.to_le_bytes());
    body.extend_from_slice(&0x0001_u32.to_le_bytes());
    body.extend_from_slice(&1_u32.to_le_bytes()); // Repeat::Normal
    b.render_change_picture(None, pic_xid, &body)
        .expect("change_picture");
    match b.core.pictures.get(&pic_xid) {
        Some(PictureRecord::Drawable { repeat, .. }) => {
            assert_eq!(*repeat, Repeat::Normal);
        }
        _ => panic!("record dropped"),
    }

    // FreePicture removes the record.
    b.render_free_picture(None, pic_xid).expect("free_picture");
    assert!(!b.core.pictures.contains_key(&pic_xid));
}

/// `picture_record_drawable_refcount` per plan §3b: a picture
/// wrapping a pixmap incref's the pixmap on create; the pixmap
/// survives `free_pixmap` while a picture still references it;
/// `render_free_picture` decref's, allowing the pending retire
/// to complete on the next poll.
#[test]
fn picture_record_drawable_refcount_blocks_free_pixmap() {
    use ash::vk;

    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    // The `for_tests` fixture has no VkContext, so the
    // production `create_pixmap` path falls back to a logged
    // gap (no storage allocated). Use the store's test-stub
    // path directly so refcount accounting is exercised
    // without needing a live Vk.
    let pix_xid = 0xDEAD_BABE;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let pix_id = b
        .store
        .allocate(pix_xid, DrawableKind::Pixmap, 32, false, storage)
        .expect("store allocate");
    assert_eq!(b.store.get(pix_id).expect("entry").refcount, 1);

    // Create a picture wrapping the pixmap; refcount → 2.
    let pix_handle = PixmapHandle::from_raw(pix_xid).expect("PixmapHandle");
    let any = AnyHandle::Pixmap(pix_handle);
    let pic = b
        .render_create_picture(None, any, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();
    assert_eq!(b.store.get(pix_id).expect("entry").refcount, 2);

    // free_pixmap drops one ref → 1; the entry survives because
    // the picture still references it.
    b.free_pixmap(None, pix_xid).expect("free_pixmap");
    assert_eq!(b.store.get(pix_id).expect("entry survives").refcount, 1);

    // free_picture drops the second ref → 0; the entry retires.
    // The test-stub storage has no in-flight fence, so
    // `destroy_now` runs immediately and the entry is removed.
    b.render_free_picture(None, pic_xid).expect("free_picture");
    assert!(b.store.get(pix_id).is_none(), "entry destroyed on last ref");
}

/// The store-refcount backstop only fires when the backing was
/// already materialized at picture-create time. When the Picture
/// wraps a host xid whose store entry is allocated LATER (window
/// map + redirect before backing alloc, GLX-TFP / Present / DRI3
/// import), `render_create_picture` takes no incref — a later
/// `free_pixmap` reaches refcount 0 and destroys the drawable out
/// from under the live Picture → transparent window (game-start
/// transparency bug). The picture must acquire the store ref as
/// soon as the backing materializes.
#[test]
fn picture_before_backing_pins_backing_on_late_materialization() {
    use ash::vk;

    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix_xid = 0xDEAD_BEEF;

    // Picture wraps the xid BEFORE the backing exists in the store.
    let pix_handle = PixmapHandle::from_raw(pix_xid).expect("PixmapHandle");
    let any = AnyHandle::Pixmap(pix_handle);
    let pic = b
        .render_create_picture(None, any, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Backing materializes later (owning refcount 1). Materializing
    // must apply the deferred picture ref.
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let pix_id = b
        .store
        .allocate(pix_xid, DrawableKind::Pixmap, 32, false, storage)
        .expect("store allocate");
    b.apply_pending_picture_refs(pix_xid);
    assert_eq!(
        b.store.get(pix_id).expect("entry").refcount,
        2,
        "picture must hold a store ref once the backing materializes"
    );

    // free_pixmap must NOT destroy: the picture still references
    // the drawable.
    b.free_pixmap(None, pix_xid).expect("free_pixmap");
    assert_eq!(
        b.store.get(pix_id).expect("backing survives").refcount,
        1,
        "picture must pin the backing through free_pixmap"
    );

    // Freeing the picture releases the last ref → destroyed.
    b.render_free_picture(None, pic_xid).expect("free_picture");
    assert!(b.store.get(pix_id).is_none(), "entry destroyed on last ref");
}

/// A picture freed BEFORE its backing ever materializes must not
/// leave a deferred ref behind: no incref was ever taken, so
/// `render_free_picture` just drops the pending entry and a later
/// `free_pixmap` on the freshly allocated backing reaches 0 and
/// destroys it normally.
#[test]
fn picture_freed_before_materialization_leaves_no_pending_ref() {
    use ash::vk;

    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix_xid = 0xDEAD_BABE;

    let pix_handle = PixmapHandle::from_raw(pix_xid).expect("PixmapHandle");
    let any = AnyHandle::Pixmap(pix_handle);
    let pic = b
        .render_create_picture(None, any, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Free the picture before the backing materializes: the
    // deferred ref must be dropped, no decref applied.
    b.render_free_picture(None, pic_xid).expect("free_picture");
    assert!(
        !b.pending_picture_drawable_refs.contains_key(&pic_xid),
        "pending ref must be dropped on early picture free"
    );

    // Backing materializes later with no pending picture refs.
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let pix_id = b
        .store
        .allocate(pix_xid, DrawableKind::Pixmap, 32, false, storage)
        .expect("store allocate");
    b.apply_pending_picture_refs(pix_xid);
    assert_eq!(b.store.get(pix_id).expect("entry").refcount, 1);

    // The lone owning ref is dropped by free_pixmap → destroyed.
    b.free_pixmap(None, pix_xid).expect("free_pixmap");
    assert!(b.store.get(pix_id).is_none(), "entry destroyed on last ref");
}

/// A window Picture takes no store reference: when the window's
/// storage is detached and replaced under the same xid (reconfigure
/// today, unmap/remap under the window-storage lifecycle), the old
/// storage dies with its owner ref, the Picture resolves to the new
/// storage, and freeing the Picture leaves that storage untouched.
#[test]
fn window_picture_holds_no_store_ref_and_follows_rebind() {
    use ash::vk;

    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::{AnyHandle, WindowHandle};

    let mut b = KmsBackend::for_tests();
    let window_xid = 0x400230;
    let alloc = |b: &mut KmsBackend, width| {
        b.store
            .allocate(
                window_xid,
                DrawableKind::Window,
                24,
                true,
                Storage::for_tests_null(
                    vk::Extent2D { width, height: 32 },
                    vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("allocate window storage")
    };
    let old_id = alloc(&mut b, 64);

    let picture = b
        .render_create_picture(
            None,
            AnyHandle::Window(WindowHandle::from_raw(window_xid).expect("WindowHandle")),
            0,
            0,
            &[],
        )
        .expect("create_picture")
        .expect("Some(handle)");
    let pic_xid = picture.as_raw();
    assert_eq!(b.store.get(old_id).expect("old entry").refcount, 1);
    assert!(!b.pending_picture_drawable_refs.contains_key(&pic_xid));

    // Mirror configure_subwindow's detach + owner decref.
    b.store.detach_xid(window_xid);
    b.store_decref_with_invalidate(old_id);
    assert!(
        b.store.get(old_id).is_none(),
        "the Picture must not keep the old window storage alive",
    );
    let new_id = alloc(&mut b, 1565);
    assert_eq!(
        b.store.get(new_id).expect("new entry").refcount,
        1,
        "materializing window storage must not apply a picture ref",
    );
    assert_eq!(
        b.resolve_paint_target(window_xid).map(|t| t.backing_id()),
        Some(new_id),
        "the Picture's drawable resolves to the replacement storage",
    );

    b.render_free_picture(None, pic_xid).expect("free_picture");
    assert_eq!(b.store.lookup(window_xid), Some(new_id));
    assert_eq!(
        b.store.get(new_id).expect("replacement entry").refcount,
        1,
        "freeing a window Picture must not decref the window storage",
    );
}

/// A window Picture draws into whatever storage the window has at the
/// time of use: the reallocated leaf after a resize, nothing (no error)
/// while the window has no storage, and a freshly allocated leaf after.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn window_picture_follows_window_storage_and_clips_away_without_it() {
    use yserver_core::{
        backend::{AnyHandle, Backend, WindowHandle},
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };

    use crate::kms::render::store::DrawableKind;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let rect = |w: u16, h: u16| {
        let mut r = Vec::new();
        r.extend_from_slice(&0i16.to_le_bytes());
        r.extend_from_slice(&0i16.to_le_bytes());
        r.extend_from_slice(&w.to_le_bytes());
        r.extend_from_slice(&h.to_le_bytes());
        r
    };
    let red = [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF];
    let blue = [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
    let assert_all = |b: &mut KmsBackend, xid: u32, n: u16, bgr: [u8; 3], what: &str| {
        let px = b
            .get_image_pixels_for_tests(xid, 2, 0, 0, n, n, !0)
            .expect("get_image")
            .expect("pixels");
        for (i, p) in px.chunks_exact(4).enumerate() {
            assert_eq!(&p[..3], &bgr, "{what}: pixel {i}");
        }
    };

    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("window");
    let w_xid = w.as_raw();
    b.map_window_for_tests(w_xid).expect("map");
    let pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("create_picture")
        .expect("Some")
        .as_raw();
    let first = b.store.lookup(w_xid).expect("leaf");
    assert_eq!(b.store.get(first).unwrap().refcount, 1, "no picture ref");
    b.render_fill_rectangles(None, pic, 1, red, &rect(8, 8), 0, 0)
        .expect("fill");
    assert_all(&mut b, w_xid, 8, [0, 0, 0xFF], "first leaf");

    b.configure_subwindow(
        None,
        w_xid,
        HostSubwindowConfig {
            x: None,
            y: None,
            width: Some(16),
            height: Some(16),
            border_width: None,
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("resize");
    let second = b.store.lookup(w_xid).expect("resized leaf");
    assert_ne!(second, first, "resize reallocates the leaf");
    b.render_fill_rectangles(None, pic, 1, red, &rect(16, 16), 0, 0)
        .expect("fill");
    assert_all(&mut b, w_xid, 16, [0, 0, 0xFF], "reallocated leaf");

    // No storage: the Picture's ops are clipped away, not errors.
    b.store.detach_xid(w_xid);
    b.store_decref_with_invalidate(second);
    assert!(b.store.lookup(w_xid).is_none());
    b.render_fill_rectangles(None, pic, 1, red, &rect(16, 16), 0, 0)
        .expect("fill with no storage");
    let pix = b.create_pixmap(None, 24, 4, 4).expect("pixmap");
    let pix_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(pix), 0, 0, &[])
        .expect("create_picture")
        .expect("Some")
        .as_raw();
    b.render_fill_rectangles(None, pix_pic, 1, blue, &rect(4, 4), 0, 0)
        .expect("fill pixmap");
    let painted = b
        .render_composite(None, 1, pic, 0, pix_pic, 0, 0, 0, 0, 0, 0, 4, 4)
        .expect("composite from a window without storage");
    assert!(painted.is_empty());
    assert_all(&mut b, pix.as_raw(), 4, [0xFF, 0, 0], "pixmap untouched");

    // New storage: the same Picture binds to it.
    let storage = b
        .platform
        .allocate_drawable_storage_as(
            16,
            16,
            24,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        )
        .expect("storage");
    let third = b
        .store_alloc(w_xid, DrawableKind::Window, 24, true, storage)
        .expect("store_alloc");
    assert_eq!(b.store.get(third).unwrap().refcount, 1, "no picture ref");
    b.render_fill_rectangles(None, pic, 1, red, &rect(16, 16), 0, 0)
        .expect("fill");
    assert_all(&mut b, w_xid, 16, [0, 0, 0xFF], "new leaf");
    b.render_free_picture(None, pic).expect("free_picture");
    assert_eq!(b.store.get(third).unwrap().refcount, 1);
}

/// Window-storage step 5: a window Picture draws nothing while hidden, then binds to the new leaf.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn window_picture_rebinds_to_the_new_leaf_after_remap() {
    use yserver_core::{
        backend::{AnyHandle, Backend, WindowHandle},
        host_x11::HostSubwindowVisual,
    };
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let rect = {
        let mut r = Vec::new();
        r.extend_from_slice(&0i16.to_le_bytes());
        r.extend_from_slice(&0i16.to_le_bytes());
        r.extend_from_slice(&8u16.to_le_bytes());
        r.extend_from_slice(&8u16.to_le_bytes());
        r
    };
    let red = [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF];
    let blue = [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
    let assert_all = |b: &mut KmsBackend, xid: u32, bgr: [u8; 3], what: &str| {
        let px = b
            .get_image_pixels_for_tests(xid, 2, 0, 0, 8, 8, !0)
            .expect("get_image")
            .expect("pixels");
        for (i, p) in px.chunks_exact(4).enumerate() {
            assert_eq!(&p[..3], &bgr, "{what}: pixel {i}");
        }
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(0),
            None,
        )
        .expect("window");
    let w_xid = w.as_raw();
    let pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("create_picture")
        .expect("Some")
        .as_raw();
    b.render_fill_rectangles(None, pic, 1, red, &rect, 0, 0)
        .expect("fill before the first map");
    assert!(
        b.store.lookup(w_xid).is_none(),
        "an unmapped window has no leaf"
    );

    b.map_window_for_tests(w_xid).expect("map");
    let first = b.store.lookup(w_xid).expect("leaf after map");
    assert_all(
        &mut b,
        w_xid,
        [0, 0, 0],
        "the first map tiles the background",
    );
    b.render_fill_rectangles(None, pic, 1, red, &rect, 0, 0)
        .expect("fill");
    assert_all(&mut b, w_xid, [0, 0, 0xFF], "first leaf");

    b.unmap_window_for_tests(w_xid).expect("unmap");
    assert!(b.store.lookup(w_xid).is_none(), "unmap releases the leaf");
    b.render_fill_rectangles(None, pic, 1, blue, &rect, 0, 0)
        .expect("fill while unmapped is clipped away");

    b.map_window_for_tests(w_xid).expect("remap");
    let second = b.store.lookup(w_xid).expect("leaf after remap");
    assert_ne!(first, second, "the remap allocates a fresh leaf");
    assert_all(&mut b, w_xid, [0, 0, 0], "the hidden fill wrote nothing");
    b.render_fill_rectangles(None, pic, 1, blue, &rect, 0, 0)
        .expect("fill");
    assert_all(
        &mut b,
        w_xid,
        [0xFF, 0, 0],
        "the Picture binds to the new leaf",
    );
    assert!(
        b.logged_gaps.borrow().is_empty(),
        "no render gap was logged"
    );
}

/// Window-storage step 5: a draw to an unmapped window writes nothing, not even into a backing.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn drawing_to_an_unmapped_window_writes_nothing_even_under_a_redirected_parent() {
    use yserver_core::{
        backend::{Backend, WindowHandle},
        host_x11::HostSubwindowVisual,
    };
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let visual = HostSubwindowVisual::Explicit {
        depth: 24,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(None, root, 0, 0, 16, 16, 0, visual, Some(0), None)
        .expect("W");
    let w_xid = w.as_raw();
    let c = b
        .create_subwindow(None, w, 4, 4, 8, 8, 0, visual, None, None)
        .expect("C");
    let c_xid = c.as_raw();
    b.map_window_for_tests(w_xid).expect("map W");
    let all_of = |b: &mut KmsBackend, xid: u32| {
        let (_, _, px) = b.backing_pixels_for_tests(xid).expect("pixels");
        let first: [u8; 3] = px[..3].try_into().expect("pixel");
        assert!(
            px.chunks_exact(4).all(|p| p[..3] == first),
            "0x{xid:x} is not uniform"
        );
        first
    };
    let hidden_draws = |b: &mut KmsBackend| {
        b.fill_rectangle(None, c_xid, 0x00FF_0000, 0, 0, 8, 8)
            .expect("fill");
        b.clear_area(None, c_xid, 0x0000_FF00, None, 0, 0, 8, 8, (0, 0))
            .expect("clear_area");
        b.change_subwindow_attributes(None, c_xid, 0x08, &[0x0000_00FF])
            .expect("border pixel");
        b.copy_area(None, w_xid, c_xid, 0, 0, 0, 0, 8, 8)
            .expect("copy into C");
        b.copy_area(None, c_xid, w_xid, 0, 0, 0, 0, 8, 8)
            .expect("copy from C");
    };

    assert!(b.store.lookup(c_xid).is_none(), "C was never viewable");
    hidden_draws(&mut b);
    assert!(b.store.lookup(c_xid).is_none(), "drawing allocates nothing");
    assert_eq!(all_of(&mut b, w_xid), [0, 0, 0], "W untouched");

    let backing = b
        .allocate_redirected_backing(None, w, 16, 16, 24)
        .expect("redirect W");
    b.fill_rectangle(None, w_xid, 0x0000_FF00, 0, 0, 16, 16)
        .expect("paint the backing");
    assert!(
        b.resolve_paint_target(c_xid).is_none(),
        "hidden C resolves to None"
    );
    hidden_draws(&mut b);
    assert_eq!(all_of(&mut b, w_xid), [0, 0xFF, 0], "backing untouched");

    b.map_window_for_tests(c_xid).expect("map C");
    assert_eq!(
        b.resolve_paint_target(c_xid).map(|t| t.backing_id()),
        b.store.lookup(backing.as_raw()),
        "a viewable C paints into W's backing",
    );
    b.unmap_window_for_tests(c_xid).expect("unmap C");
    assert!(b.store.lookup(c_xid).is_none(), "unmap releases C's leaf");
    assert!(
        b.resolve_paint_target(c_xid).is_none(),
        "unmapped C resolves to None"
    );
    hidden_draws(&mut b);
    assert_eq!(
        all_of(&mut b, w_xid),
        [0, 0xFF, 0],
        "backing still untouched"
    );
    assert!(
        b.logged_gaps.borrow().is_empty(),
        "no render gap was logged"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn damage_boundary_submits_render_batches_and_queued_commands() {
    use crate::kms::{
        cpu_types::Repeat,
        render::{
            engine::{RenderFlushReason, ResolvedSource, SourceDrawable},
            target::Dst,
        },
        vk::ops::render::CompositeRect,
    };
    use yserver_core::backend::Backend;

    for already_queued in [false, true] {
        let mut b = KmsBackend::for_tests_with_vk().expect("live Vulkan ICD");
        let src = b.create_pixmap(None, 32, 4, 4).expect("source");
        let dst = b.create_pixmap(None, 32, 4, 4).expect("destination");
        b.engine
            .close_open_frame(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::frame_builder::CloseReason::SyncWait,
            )
            .expect("submit initial clears");
        b.platform.submit_group_set_max_size_for_tests(16);
        let src_id = b.store.lookup(src.as_raw()).expect("source storage");
        let dst_id = b.store.lookup(dst.as_raw()).expect("destination storage");
        let appended = b
            .engine
            .try_append_render_batch(
                &mut b.store,
                &mut b.platform,
                1, // PictOpSrc
                ResolvedSource::Drawable(SourceDrawable::whole(src_id)),
                ResolvedSource::None,
                Dst::server_internal(dst_id),
                &[CompositeRect {
                    src_x: 0,
                    src_y: 0,
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: 0,
                    dst_y: 0,
                    width: 4,
                    height: 4,
                }],
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .expect("record composite");
        assert!(appended.is_some(), "composite takes the batch path");
        if already_queued {
            b.engine
                .flush_render_batch(&mut b.store, &mut b.platform, RenderFlushReason::Other)
                .expect("close batch without submitting");
            assert!(b.platform.submit_group_size() > 0);
        } else {
            assert!(b.engine.has_pending_batches_for_tests());
        }

        b.flush_before_damage_notify();

        assert!(
            !b.engine.has_pending_batches_for_tests(),
            "damage boundary must close a pending render batch"
        );
        assert_eq!(
            b.platform.submit_group_size(),
            0,
            "damage boundary must submit even without an open frame"
        );
        assert_eq!(b.engine.pending_group_ops_count_for_tests(), 0);
        b.platform.wait_idle_bounded();
    }
}

/// Window-storage step 5: the root and the COW own their storage outside the lifecycle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_and_cow_storage_never_go_through_the_lifecycle() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let root = b.core.window_id;
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    assert!(b.get_overlay_window(None).expect("claim COW"));
    let root_id = b.store.lookup(root).expect("root storage");
    let cow_id = b.store.lookup(cow).expect("COW storage");
    for xid in [root, cow] {
        b.release_window_storage(None, xid).expect("release");
        b.realize_window_storage(None, xid).expect("realize");
    }
    assert_eq!(
        b.store.lookup(root),
        Some(root_id),
        "root keeps its storage"
    );
    assert_eq!(b.store.lookup(cow), Some(cow_id), "COW keeps its storage");
    assert!(
        b.resolve_paint_target(cow).is_some(),
        "the COW stays paintable"
    );
}

/// Window-storage step 5: a same-size remap takes the released leaf back from the pool.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn window_remap_at_the_same_size_is_a_pool_hit() {
    use crate::kms::vk::mem_accounting::{self, MemCategory};
    use yserver_core::{
        backend::{Backend, WindowHandle},
        host_x11::HostSubwindowVisual,
    };
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    fn settle(b: &mut KmsBackend) {
        b.engine
            .close_open_frame(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::frame_builder::CloseReason::SyncWait,
            )
            .expect("close frame");
        b.engine
            .flush_submit_group(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::submit_group::FlushReason::SyncBoundary,
            )
            .expect("flush");
        b.platform.wait_idle_bounded();
        b.poll_pending_retire_with_invalidate();
    }
    let vk = std::sync::Arc::clone(b.platform.vk.as_ref().expect("vk"));
    let pool = std::sync::Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(vk));
    b.platform.pixmap_pool = Some(std::sync::Arc::clone(&pool));
    let memory_of = |b: &KmsBackend, xid: u32| {
        let id = b.store.lookup(xid).expect("leaf");
        b.store.get(id).expect("drawable").storage.memory
    };
    // An odd extent no other test allocates: the pool and the ledger are shared.
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            83,
            79,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(0),
            None,
        )
        .expect("window")
        .as_raw();
    b.map_window_for_tests(w).expect("map");
    let mem = memory_of(&b, w);
    assert_eq!(
        mem_accounting::entry_of(mem).map(|e| e.1),
        Some(MemCategory::WindowStorage)
    );
    b.unmap_window_for_tests(w).expect("unmap");
    settle(&mut b);
    assert!(b.store.lookup(w).is_none(), "unmap released the leaf");
    assert_eq!(
        mem_accounting::entry_of(mem).map(|e| e.1),
        Some(MemCategory::PoolIdle),
        "the released leaf parks in the pool",
    );
    b.map_window_for_tests(w).expect("remap");
    assert_eq!(memory_of(&b, w), mem, "same-size remap is a pool hit");
    assert_eq!(
        mem_accounting::entry_of(mem).map(|e| e.1),
        Some(MemCategory::WindowStorage)
    );
    b.destroy_subwindow(None, w).expect("destroy");
    settle(&mut b);
    pool.drain();
}

/// #196: eviction and the idle trim really free the pooled Vk memory,
/// and the ledger stops counting it as `pool_idle`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn pixmap_pool_eviction_and_idle_trim_free_the_memory() {
    use crate::kms::vk::{
        mem_accounting::{self, MemCategory},
        pixmap_pool::{PixmapPool, PixmapPoolLimits},
    };
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    fn settle(b: &mut KmsBackend) {
        b.engine
            .close_open_frame(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::frame_builder::CloseReason::SyncWait,
            )
            .expect("close frame");
        b.engine
            .flush_submit_group(
                &mut b.store,
                &mut b.platform,
                crate::kms::render::submit_group::FlushReason::SyncBoundary,
            )
            .expect("flush");
        b.platform.wait_idle_bounded();
        b.poll_pending_retire_with_invalidate();
    }
    let memory_of = |b: &KmsBackend, xid: u32| {
        let id = b.store.lookup(xid).expect("pixmap in store");
        b.store.get(id).expect("drawable").storage.memory
    };
    // Odd extents no other test allocates: the ledger is process-global.
    let sizes: [(u16, u16); 4] = [(97, 89), (101, 83), (103, 79), (107, 73)];
    let entry_bytes = |w: u16, h: u16| u64::from(w) * u64::from(h) * 4;
    // Room for about two entries; the third return crosses the budget
    // and evicts down to one.
    let budget = 2 * entry_bytes(107, 73) * 2;
    let vk = std::sync::Arc::clone(b.platform.vk.as_ref().expect("vk"));
    let pool = std::sync::Arc::new(PixmapPool::with_limits(
        vk,
        PixmapPoolLimits {
            budget_bytes: budget,
            low_water_bytes: budget / 2,
            ..PixmapPoolLimits::default()
        },
    ));
    b.platform.pixmap_pool = Some(std::sync::Arc::clone(&pool));
    let xids: Vec<u32> = sizes
        .iter()
        .map(|&(w, h)| b.create_pixmap(None, 32, w, h).expect("pixmap").as_raw())
        .collect();
    let mems: Vec<_> = xids.iter().map(|&x| memory_of(&b, x)).collect();
    let real: u64 = mems
        .iter()
        .map(|&m| mem_accounting::entry_of(m).expect("tracked").0)
        .sum();
    for &x in &xids {
        b.free_pixmap(None, x).expect("free");
        settle(&mut b);
    }
    let held: Vec<_> = mems
        .iter()
        .filter(|&&m| mem_accounting::entry_of(m).map(|e| e.1) == Some(MemCategory::PoolIdle))
        .copied()
        .collect();
    let gone = mems
        .iter()
        .filter(|&&m| mem_accounting::entry_of(m).is_none())
        .count();
    assert_eq!(held.len() + gone, mems.len(), "each one parked or freed");
    assert!(gone > 0, "the budget evicted something ({real} B returned)");
    assert_eq!(
        held.last(),
        mems.last(),
        "the most recent return survives eviction"
    );
    let r = pool.residency();
    let held_bytes: u64 = held
        .iter()
        .map(|&m| mem_accounting::entry_of(m).expect("held").0)
        .sum();
    assert_eq!(r.entries as usize, held.len());
    assert_eq!(r.bytes, held_bytes, "budgeted on the real allocation size");
    assert!(r.bytes <= budget);
    assert_eq!(pool.stats().total_evicted_budget as usize, gone);

    // The idle trim frees the rest once they outlive the age.
    let due = pool.next_trim_deadline().expect("entries held");
    pool.trim_idle(due - std::time::Duration::from_secs(5));
    assert_eq!(pool.residency().entries as usize, held.len(), "not yet due");
    pool.trim_idle(due);
    assert_eq!(pool.residency(), Default::default());
    assert_eq!(pool.next_trim_deadline(), None);
    for &m in &held {
        assert_eq!(mem_accounting::entry_of(m), None, "trimmed memory freed");
    }
    assert_eq!(pool.stats().total_evicted_idle as usize, held.len());
}

/// `picture_solid_fill_premul_correct` per plan §3b. NB: the
/// X RENDER wire colour is **already premultiplied** per the
/// protocol + rendercheck (`main.c:337-345`), so v2 stores the
/// channels as-is rather than multiplying by alpha. The plan's
/// `0x80808080 → [0.25, 0.25, 0.25, 0.5]` example assumed
/// straight-alpha input; v1 has been parity with rendercheck
/// since Phase 4.1.4.6, and v2 matches v1.
#[test]
fn render_create_solid_fill_stores_wire_color_as_is() {
    // Wire colour: r16=0xFFFF (1.0), g16=0x8080 (≈0.50196),
    // b16=0x0000 (0.0), a16=0x8080 (≈0.50196). Stored f32
    // values should be (r=1.0, g=0.5019, b=0.0, a=0.5019)
    // exactly — no premultiplication applied at store time.
    let mut b = KmsBackend::for_tests();
    let color: [u8; 8] = [0xFF, 0xFF, 0x80, 0x80, 0x00, 0x00, 0x80, 0x80];
    let pic = b
        .render_create_solid_fill(None, color)
        .expect("solid_fill")
        .expect("Some");
    let rec = b.core.pictures.get(&pic.as_raw()).expect("record");
    match rec {
        PictureRecord::SolidFill {
            premul,
            repeat,
            component_alpha,
        } => {
            assert!((premul[0] - 1.0).abs() < 1e-4, "r = {}", premul[0]);
            assert!(
                (premul[1] - (0x8080_u16 as f32 / 65535.0)).abs() < 1e-6,
                "g = {}",
                premul[1],
            );
            assert!(premul[2].abs() < 1e-6, "b = {}", premul[2]);
            assert!(
                (premul[3] - (0x8080_u16 as f32 / 65535.0)).abs() < 1e-6,
                "a = {}",
                premul[3],
            );
            // Solid-fill defaults to Repeat::Normal; component_alpha=false.
            assert_eq!(*repeat, Repeat::Normal);
            assert!(!*component_alpha);
        }
        other => panic!("expected SolidFill, got {other:?}"),
    }
}

/// `picture_gradient_record_stored` per plan §3b: a linear
/// gradient body parses; endpoints + stops round-trip through
/// the record.
#[test]
fn render_create_linear_gradient_parses_endpoints_and_stops() {
    let mut b = KmsBackend::for_tests();
    // Wire body: pad(4) + p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4)
    // + n_stops(4) + n*pos(4) + n*color(8).
    // p1 = (0, 0) fixed-point; p2 = (256<<16, 0); two stops at
    // pos=0 with color=(0xFFFF, 0, 0, 0xFFFF) and pos=1<<16 with
    // color=(0, 0xFFFF, 0, 0xFFFF).
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&0_u32.to_le_bytes()); // request padding (skipped)
    body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
    body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
    body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
    body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
    body.extend_from_slice(&2_u32.to_le_bytes()); // n_stops
    // positions
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&0x0001_0000_i32.to_le_bytes());
    // colours
    body.extend_from_slice(&0xFFFF_u16.to_le_bytes()); // r0
    body.extend_from_slice(&0_u16.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes());
    body.extend_from_slice(&0xFFFF_u16.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes()); // r1=0
    body.extend_from_slice(&0xFFFF_u16.to_le_bytes()); // g1
    body.extend_from_slice(&0_u16.to_le_bytes());
    body.extend_from_slice(&0xFFFF_u16.to_le_bytes());

    let pic = b
        .render_create_linear_gradient(None, &body)
        .expect("linear_gradient")
        .expect("Some");
    let rec = b.core.pictures.get(&pic.as_raw()).expect("record");
    match rec {
        PictureRecord::LinearGradient {
            p1,
            p2,
            stops,
            repeat,
            transform,
        } => {
            assert_eq!(*p1, (0, 0));
            assert_eq!(*p2, (256 << 16, 0));
            assert_eq!(stops.len(), 2);
            assert_eq!(stops[0].pos, 0);
            assert_eq!(stops[0].r, 0xFFFF);
            assert_eq!(stops[0].g, 0);
            assert_eq!(stops[1].pos, 0x0001_0000);
            assert_eq!(stops[1].g, 0xFFFF);
            assert_eq!(*repeat, Repeat::None);
            assert!(transform.is_none());
        }
        other => panic!("expected LinearGradient, got {other:?}"),
    }
}

/// Stage 3f.13: `render_create_linear_gradient` returns a
/// resolved `ResolvedSource::Gradient(xid)` from
/// `resolve_picture_for_render` (not a SolidFill collapse).
/// Logic-only — engine-side LUT build is a Vk path and lives
/// in the engine's Vk-backed tests; here we just assert the
/// resolve shape changed correctly.
#[test]
fn linear_gradient_resolves_as_gradient_source() {
    use crate::kms::render::engine::ResolvedSource;

    let mut b = KmsBackend::for_tests();
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&0_u32.to_le_bytes()); // pad
    body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
    body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
    body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
    body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
    body.extend_from_slice(&1_u32.to_le_bytes()); // n_stops
    body.extend_from_slice(&0_i32.to_le_bytes()); // pos
    body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]); // colour (R=1, A=1)
    let pic = b
        .render_create_linear_gradient(None, &body)
        .expect("create gradient")
        .expect("Some");

    let (resolved, _, _, _) = b.resolve_picture_for_render(pic.as_raw()).expect("resolve");
    match resolved {
        ResolvedSource::Gradient(xid) => assert_eq!(xid, pic.as_raw()),
        other => panic!("expected Gradient, got {other:?}"),
    }
}

/// Stage 3f.13: `render_free_picture` for a gradient drops both
/// the picture record and the engine-side `picture_paint` slot.
/// Logic-only — the engine slot count is the observable signal
/// (`engine.picture_paint_len()`). On the test fixture (no Vk)
/// the build itself logs a debug + skips, so the engine slot
/// stays at 0 throughout; the gate is "free_picture doesn't
/// leave a stale slot behind" which still asserts non-zero in
/// production but zero in test. We assert the lifecycle path
/// instead: create, free, ensure picture record is gone.
#[test]
fn gradient_free_picture_drops_record() {
    let mut b = KmsBackend::for_tests();
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&0_u32.to_le_bytes()); // pad
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&(128_i32 << 16).to_le_bytes());
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&1_u32.to_le_bytes());
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]);
    let pic = b
        .render_create_linear_gradient(None, &body)
        .expect("create gradient")
        .expect("Some");
    let xid = pic.as_raw();
    assert!(b.core.pictures.contains_key(&xid));
    b.render_free_picture(None, xid).expect("free");
    assert!(!b.core.pictures.contains_key(&xid));
    assert_eq!(b.engine.picture_paint_len(), 0);
}

/// Stage 3f.14: depth-32 windows are premultiplied-α and a
/// transparent-black default is the no-op contribution to
/// compositing; depth-24 (and other non-α visuals) get opaque
/// black. Locks the contract in the test suite so a refactor
/// doesn't silently flip 32-bit windows to opaque black (which
/// would visually look the same on top of the root but break
/// compositors that depend on alpha for blending).
#[test]
fn default_window_init_color_per_depth() {
    assert_eq!(
        crate::kms::render::backend::default_window_init_color(32),
        [0.0, 0.0, 0.0, 0.0]
    );
    assert_eq!(
        crate::kms::render::backend::default_window_init_color(24),
        [0.0, 0.0, 0.0, 1.0]
    );
    assert_eq!(
        crate::kms::render::backend::default_window_init_color(1),
        [0.0, 0.0, 0.0, 1.0]
    );
    assert_eq!(
        crate::kms::render::backend::default_window_init_color(8),
        [0.0, 0.0, 0.0, 1.0]
    );
}

/// `render_set_picture_clip_rectangles` parses + stores rects
/// pre-shifted by the clip-origin. Then `render_free_picture`
/// teardown also drops the engine-side picture_paint slot.
#[test]
fn set_picture_clip_rectangles_pre_shifts_by_origin() {
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA00_BB00).expect("PixmapHandle"));
    let pic = b
        .render_create_picture(None, drawable, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Wire body: picture(4) + x_origin(2) + y_origin(2) +
    // 1 × [x=5, y=6, w=20, h=30].
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&pic_xid.to_le_bytes());
    body.extend_from_slice(&10_i16.to_le_bytes()); // x_origin
    body.extend_from_slice(&20_i16.to_le_bytes()); // y_origin
    body.extend_from_slice(&5_i16.to_le_bytes());
    body.extend_from_slice(&6_i16.to_le_bytes());
    body.extend_from_slice(&20_u16.to_le_bytes());
    body.extend_from_slice(&30_u16.to_le_bytes());
    b.render_set_picture_clip_rectangles(None, pic_xid, &body)
        .expect("set_picture_clip");
    // Pre-shift: stored rect.x = 5 + 10 = 15; .y = 6 + 20 = 26.
    match b.core.pictures.get(&pic_xid).expect("rec") {
        PictureRecord::Drawable {
            clip,
            clip_x,
            clip_y,
            ..
        } => {
            let rects = clip.as_ref().expect("Some(rects)");
            assert_eq!(rects.len(), 1);
            assert_eq!(rects[0].x, 15);
            assert_eq!(rects[0].y, 26);
            assert_eq!(rects[0].width, 20);
            assert_eq!(rects[0].height, 30);
            assert_eq!(*clip_x, 10);
            assert_eq!(*clip_y, 20);
        }
        _ => panic!("not Drawable"),
    }

    // free_picture removes both record + engine-side slot.
    assert_eq!(b.engine.picture_paint_len(), 0);
    b.render_free_picture(None, pic_xid).expect("free");
    assert!(!b.core.pictures.contains_key(&pic_xid));
    assert_eq!(b.engine.picture_paint_len(), 0);
}

/// X11 RENDER `SetPictureClipRectangles` with an EMPTY rect
/// list = empty clip region = composite paints **nothing**.
/// Distinct from `ChangePicture(CPClipMask = None)` which
/// clears the clip back to "paint everywhere" (`clip = None`).
///
/// Regression: pre-fix v2 collapsed empty-list to `clip = None`,
/// which made subsequent composites paint everywhere — exactly
/// the mate-with-compositing "shadow only / wallpaper
/// overwrites window content" bug observed in the Stage 4d
/// smoke. The trace at 09:49:44 showed marco's wallpaper-fill
/// composite running with `clip[]` (= `None` in v2 storage)
/// even though marco's intent (per X11 spec) was "empty clip,
/// paint nothing."
///
/// Post-fix: empty rect list stores `Some(Vec::new())` so the
/// engine's `clip_rects=Some(&[])` path returns early without
/// painting.
#[test]
fn set_picture_clip_rectangles_empty_list_is_empty_clip_not_no_clip() {
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0xCC00_DD00).expect("PixmapHandle"));
    let pic = b
        .render_create_picture(None, drawable, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // First: set a real clip to prove the field can become
    // populated (Some(non-empty)).
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&pic_xid.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
    body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    b.render_set_picture_clip_rectangles(None, pic_xid, &body)
        .expect("set_clip non-empty");
    match b.core.pictures.get(&pic_xid).expect("rec") {
        PictureRecord::Drawable { clip, .. } => {
            assert!(
                matches!(clip, Some(v) if v.len() == 1),
                "expected Some(1 rect) after non-empty set, got {clip:?}",
            );
        }
        _ => panic!("not Drawable"),
    }

    // Now: empty list. Per X11 RENDER spec this means "empty
    // clip region — paint nothing." The stored representation
    // must distinguish this from "no clip set" (paint
    // everywhere).
    let mut empty_body: Vec<u8> = Vec::new();
    empty_body.extend_from_slice(&pic_xid.to_le_bytes());
    empty_body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
    empty_body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
    // No rect data — empty list.
    b.render_set_picture_clip_rectangles(None, pic_xid, &empty_body)
        .expect("set_clip empty");
    match b.core.pictures.get(&pic_xid).expect("rec") {
        PictureRecord::Drawable { clip, .. } => {
            // Pre-fix: clip was None (= paint everywhere).
            // Post-fix: Some(empty Vec) (= paint nothing).
            assert!(
                matches!(clip, Some(v) if v.is_empty()),
                "empty rect list must store Some(empty Vec) — \
                     pre-fix stored None which made composites paint \
                     everywhere instead of nothing. Got: {clip:?}",
            );
        }
        _ => panic!("not Drawable"),
    }
}

/// `ChangePicture(CPClipXOrigin/CPClipYOrigin)` must move the
/// already stored clip rectangles by the same delta. The v2
/// backend stores clip rects pre-shifted into picture-local
/// coordinates, so updating only the scalar origin fields leaves
/// stale scissors behind.
#[test]
fn change_picture_clip_origin_repositions_stored_rects() {
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0xDD00_EE00).expect("PixmapHandle"));
    let pic = b
        .render_create_picture(None, drawable, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    let mut clip_body: Vec<u8> = Vec::new();
    clip_body.extend_from_slice(&pic_xid.to_le_bytes());
    clip_body.extend_from_slice(&10_i16.to_le_bytes());
    clip_body.extend_from_slice(&20_i16.to_le_bytes());
    clip_body.extend_from_slice(&5_i16.to_le_bytes());
    clip_body.extend_from_slice(&6_i16.to_le_bytes());
    clip_body.extend_from_slice(&20_u16.to_le_bytes());
    clip_body.extend_from_slice(&30_u16.to_le_bytes());
    b.render_set_picture_clip_rectangles(None, pic_xid, &clip_body)
        .expect("set clip");

    let mut change_body: Vec<u8> = Vec::new();
    change_body.extend_from_slice(&pic_xid.to_le_bytes());
    change_body.extend_from_slice(&(0x0010_u32 | 0x0020_u32).to_le_bytes());
    change_body.extend_from_slice(&30_u32.to_le_bytes());
    change_body.extend_from_slice(&50_u32.to_le_bytes());
    b.render_change_picture(None, pic_xid, &change_body)
        .expect("change_picture");

    match b.core.pictures.get(&pic_xid).expect("rec") {
        PictureRecord::Drawable {
            clip,
            clip_x,
            clip_y,
            ..
        } => {
            let rects = clip.as_ref().expect("clip still present");
            assert_eq!(rects.len(), 1);
            assert_eq!(rects[0].x, 35, "x must move by +20 with CPClipXOrigin");
            assert_eq!(rects[0].y, 56, "y must move by +30 with CPClipYOrigin");
            assert_eq!(*clip_x, 30);
            assert_eq!(*clip_y, 50);
        }
        _ => panic!("not Drawable"),
    }
}

// ─── Audit #8 (2026-05-19): set_picture_drawable_origin +
// picture_client_clip_rects v2 backend hooks ──────────────

/// `set_picture_drawable_origin` writes into the
/// `PictureRecord::Drawable.drawable_origin` field. Pre-fix v2
/// inherited the trait default no-op so the field stayed at
/// (0, 0); window-backed pictures whose drawable sits at a
/// non-zero parent offset couldn't translate external region
/// geometry back into picture-local coords.
#[test]
fn set_picture_drawable_origin_persists_on_record() {
    use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA01_BB01).expect("PixmapHandle"));
    let pic = b
        .render_create_picture(None, drawable, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Pre-call sanity: default origin must be (0, 0).
    match b.core.pictures.get(&pic_xid).expect("rec") {
        PictureRecord::Drawable {
            drawable_origin, ..
        } => assert_eq!(*drawable_origin, (0, 0)),
        _ => panic!("not Drawable"),
    }

    b.set_picture_drawable_origin(pic_xid, (15, 27));

    match b.core.pictures.get(&pic_xid).expect("rec") {
        PictureRecord::Drawable {
            drawable_origin, ..
        } => {
            assert_eq!(
                *drawable_origin,
                (15, 27),
                "drawable_origin must update; pre-fix the trait default \
                     no-op left it at (0, 0)",
            );
        }
        _ => panic!("not Drawable"),
    }
}

/// `set_picture_drawable_origin` on a non-Drawable picture
/// (SolidFill / gradient) is a tolerated no-op — those variants
/// have no drawable to anchor to.
#[test]
fn set_picture_drawable_origin_no_op_on_solidfill() {
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();
    // Color is fixed-size 8 bytes (BGRA u16×4).
    let mut color = [0u8; 8];
    color[0..2].copy_from_slice(&0xFFFF_u16.to_le_bytes()); // R
    color[6..8].copy_from_slice(&0xFFFF_u16.to_le_bytes()); // A
    let pic = b
        .render_create_solid_fill(None, color)
        .expect("create solid fill")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Should not panic; record must remain a SolidFill.
    b.set_picture_drawable_origin(pic_xid, (10, 20));
    assert!(
        matches!(
            b.core.pictures.get(&pic_xid),
            Some(PictureRecord::SolidFill { .. })
        ),
        "SolidFill picture must remain SolidFill after \
             set_picture_drawable_origin no-op",
    );
}

/// `picture_client_clip_rects` on a Drawable picture WITH a
/// clip returns `Some(Some(rects))` — those rects feed
/// `CreateRegionFromPicture` (XFixes). Pre-fix v2 inherited
/// the trait default `None`, making CreateRegionFromPicture
/// always return BadMatch even for legitimate clipped pictures.
#[test]
fn picture_client_clip_rects_returns_set_clip() {
    use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA02_BB02).expect("PixmapHandle"));
    let pic = b
        .render_create_picture(None, drawable, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Install a 2-rect client clip via SetPictureClipRectangles
    // (clip-origin both zero so stored rects == request rects).
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&pic_xid.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
    body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
    for (x, y, w, h) in [(0_i16, 0_i16, 10_u16, 10_u16), (100, 200, 30, 40)] {
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&h.to_le_bytes());
    }
    b.render_set_picture_clip_rectangles(None, pic_xid, &body)
        .expect("set_clip");

    let got = b
        .picture_client_clip_rects(pic_xid)
        .expect("Drawable picture must be Some(_) (not BadMatch)");
    let rects = got.expect("clip was set, expected Some(rects)");
    assert_eq!(rects.len(), 2, "got {rects:?}");
    assert_eq!(
        (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
        (0, 0, 10, 10)
    );
    assert_eq!(
        (rects[1].x, rects[1].y, rects[1].width, rects[1].height),
        (100, 200, 30, 40)
    );
}

/// Non-zero drawable origins must not corrupt `CreateRegionFromPicture`.
/// The request path stores the origin separately, but the returned client
/// clip still needs to reflect the picture-local rectangle coordinates
/// only.
#[test]
fn picture_client_clip_rects_window_backed_picture_with_nonzero_origin() {
    use yserver_core::backend::{AnyHandle, Backend, WindowHandle};

    let mut b = KmsBackend::for_tests();
    let window_xid = 0xAA04_BB04;
    let _w_id = seed_window(&mut b, window_xid, None, 15, 27);

    let pic = b
        .render_create_picture(
            None,
            AnyHandle::Window(WindowHandle::from_raw(window_xid).expect("WindowHandle")),
            0,
            0,
            &[],
        )
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    // Mirror the request-layer origin bookkeeping that happens on CreatePicture.
    b.set_picture_drawable_origin(pic_xid, (15, 27));

    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&pic_xid.to_le_bytes());
    body.extend_from_slice(&5_i16.to_le_bytes()); // clip origin x
    body.extend_from_slice(&9_i16.to_le_bytes()); // clip origin y
    body.extend_from_slice(&1_i16.to_le_bytes());
    body.extend_from_slice(&2_i16.to_le_bytes());
    body.extend_from_slice(&7_u16.to_le_bytes());
    body.extend_from_slice(&11_u16.to_le_bytes());
    b.render_set_picture_clip_rectangles(None, pic_xid, &body)
        .expect("set_clip");

    let got = b
        .picture_client_clip_rects(pic_xid)
        .expect("Drawable picture must be Some(_) (not BadMatch)");
    let rects = got.expect("clip was set, expected Some(rects)");
    assert_eq!(rects.len(), 1, "got {rects:?}");
    assert_eq!(
        (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
        (6, 11, 7, 11),
        "drawable_origin must not be folded into CreateRegionFromPicture",
    );
}

/// `picture_client_clip_rects` on a Drawable picture with NO
/// clip set returns `Some(None)` — the picture exists but has
/// no clientClip yet. Per X RENDER /
/// `xfixes/region.c:CreateRegionFromPicture`, the dispatcher
/// then emits BadMatch on the caller (no region to extract).
#[test]
fn picture_client_clip_rects_returns_some_none_when_no_clip_set() {
    use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let drawable = AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA03_BB03).expect("PixmapHandle"));
    let pic = b
        .render_create_picture(None, drawable, 0, 0, &[])
        .expect("create_picture")
        .expect("Some");
    let pic_xid = pic.as_raw();

    let got = b.picture_client_clip_rects(pic_xid);
    assert!(
        matches!(got, Some(None)),
        "Drawable picture without a clip must return Some(None) — \
             got {got:?}",
    );
}

/// `picture_client_clip_rects` on a non-Drawable picture (e.g.,
/// SolidFill) returns the outer `None` so the protocol layer
/// raises BadMatch — gradients/solidfills carry no
/// `clientClip`. Mirrors Xorg's `CreateRegionFromPicture` →
/// BadPicture path for sourceless pictures.
#[test]
fn picture_client_clip_rects_outer_none_on_solidfill() {
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();
    let mut color = [0u8; 8];
    color[0..2].copy_from_slice(&0xFFFF_u16.to_le_bytes());
    color[6..8].copy_from_slice(&0xFFFF_u16.to_le_bytes());
    let pic = b
        .render_create_solid_fill(None, color)
        .expect("create solid fill")
        .expect("Some");
    let pic_xid = pic.as_raw();

    let got = b.picture_client_clip_rects(pic_xid);
    assert!(
        got.is_none(),
        "SolidFill picture must return outer None so the protocol \
             layer emits BadMatch — got {got:?}",
    );
}

// ─── Stage 3d: render_composite_glyphs tests ───────────────

/// Helper: install a SolidFill source picture + a glyphset
/// holding `n` 1×1 A8 glyphs at id 0..n with `0xFF` alpha.
/// Returns (src_pic_xid, gs_xid).
fn install_solidfill_and_glyphset(b: &mut KmsBackend, n: u32) -> (u32, u32) {
    use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some");

    let gs_xid = b.core.next_host_xid();
    let mut glyphs = HashMap::new();
    for id in 0..n {
        glyphs.insert(
            id,
            StoredGlyph {
                width: 1,
                height: 1,
                x: 0,
                y: 0,
                x_off: 1,
                y_off: 0,
                pixels: vec![0xFF],
                format: GlyphSetFormat::A8,
            },
        );
    }
    b.core.glyphsets.insert(
        gs_xid,
        GlyphSetState {
            format: GlyphSetFormat::A8,
            glyphs,
        },
    );
    (src_pic.as_raw(), gs_xid)
}

/// #137 visibility note, the rate limiter itself: a one-shot is
/// claimed exactly once, by whoever gets there first. Tested on a
/// LOCAL flag rather than the process-wide one, so it neither
/// depends on test order nor consumes the real one-shot.
#[test]
fn the_first_occurrence_of_a_one_shot_is_claimed_once() {
    use crate::kms::render::backend::take_first_occurrence;
    use std::sync::atomic::AtomicBool;
    let flag = AtomicBool::new(false);
    assert!(
        take_first_occurrence(&flag),
        "the first caller must get the one-shot"
    );
    for i in 0..100 {
        assert!(
            !take_first_occurrence(&flag),
            "occurrence {i} must not warn again — a pathological client \
                 would otherwise flood the log"
        );
    }
}

/// #137 visibility note: the LOG is rate-limited, the COUNTER is
/// not. A hundred unsupported requests bump the counter a hundred
/// times, so telemetry still measures how bad a gap is even after
/// the log has gone quiet.
///
/// The source here is a drawable whose sampled domain is 4x4 —
/// admissible under tier 2, not tier 1 — so the drop is the real
/// remaining one, reached without a live Vk (the read is never
/// attempted).
#[test]
fn composite_glyphs_counts_every_unsupported_drop_not_just_the_first() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use ash::vk;
    use yserver_core::backend::{AnyHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let (_unused_solidfill, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);

    // A real store entry, so the picture resolves to
    // `ResolvedSource::Drawable` rather than failing to resolve at
    // all (which is a protocol error, not an unsupported feature,
    // and deliberately does NOT bump the counter).
    let src_xid = 0x5137_0001u32;
    b.store
        .allocate(
            src_xid,
            DrawableKind::Pixmap,
            32,
            false,
            Storage::for_tests_null(
                vk::Extent2D {
                    width: 4,
                    height: 4,
                },
                vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("allocate the source pixmap");
    let src_pic = b
        .render_create_picture(
            None,
            AnyHandle::Pixmap(PixmapHandle::from_raw(src_xid).expect("PixmapHandle")),
            yserver_protocol::x11::RENDER_FMT_ARGB32,
            0x0001,              // CPRepeat
            &1u32.to_le_bytes(), // Normal
        )
        .expect("render_create_picture")
        .expect("Some")
        .as_raw();

    for _ in 0..100 {
        b.render_composite_glyphs(
            None,
            23, // CompositeGlyphs8
            3,  // Over
            src_pic,
            0xDEAD, // host_dst — the source gate fires first
            0,      // mask_fmt
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0],
            0,
            0,
        )
        .expect("ok");
    }
    assert_eq!(
        b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 100,
        "the counter is not rate-limited: every unsupported drop must advance it",
    );
    assert_eq!(
        b.telemetry.lifetime.paint_submits, 0,
        "no paint submit on the drop path",
    );
}

/// Ops outside the standard fixed-function family (0..=12) —
/// Saturate + the Disjoint/Conjoint families — still drop with
/// a per-call gap-log and increment the
/// `composite_glyphs_dropped_unsupported` lifetime counter. The
/// text pipeline has no dst-readback shader mode, so those ops
/// cannot blend fixed-function. No paint side effect; engine is
/// never reached.
#[test]
fn composite_glyphs_unsupported_op_drops() {
    let mut b = KmsBackend::for_tests();
    let (src_pic, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
    // No real dst picture needed — the op gate fires before
    // dst resolution. Pass any host_dst; assert gap-counter.
    for bad_op in [
        13, /* Saturate */
        17, /* DisjointSrc */
        33, /* ConjointSrc */
    ] {
        b.render_composite_glyphs(
            None,
            23, /* CompositeGlyphs8 */
            bad_op,
            src_pic,
            0xDEAD, /* host_dst (unused — op gate first) */
            0,      /* mask_fmt */
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], // items: 1 glyph elt + padded
            0,
            0,
        )
        .expect("ok");
    }
    assert_eq!(
        b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 3,
        "Saturate/Disjoint/Conjoint ops must bump the unsupported counter",
    );
    assert_eq!(
        b.telemetry.lifetime.paint_submits, 0,
        "no paint submit on the gap path",
    );
}

/// The cairo/Pango component-alpha text path composites glyph
/// coverage with `op=Add` into an A8 mask pixmap (then paints
/// the mask onto the window with Composite Src/OutReverse) —
/// see the i3-config-wizard black-dialog bug. Standard
/// fixed-function ops (0..=12) must reach the engine, NOT bump
/// `composite_glyphs_dropped_unsupported`. (The `for_tests`
/// fixture has no live Vk, so the engine returns NoVk without
/// painting — the gate under test is the backend op gate.)
#[test]
fn composite_glyphs_standard_ops_reach_engine() {
    let mut b = KmsBackend::for_tests();
    let (src_pic, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
    // Real dst picture wrapping an unknown drawable — resolves
    // as a Drawable picture, then short-circuits in the store
    // lookup (same shape as
    // composite_glyphs_inline_glyphset_change_parsed).
    use yserver_core::backend::{AnyHandle, PixmapHandle};
    let dst_drawable =
        AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));
    let dst_pic = b
        .render_create_picture(None, dst_drawable, 0, 0, &[])
        .expect("dst_picture")
        .expect("Some")
        .as_raw();
    for op in [
        12, /* Add — the cairo mask path */
        1,  /* Src */
        8,  /* OutReverse */
    ] {
        b.render_composite_glyphs(
            None,
            23, /* CompositeGlyphs8 */
            op,
            src_pic,
            dst_pic,
            0, /* mask_fmt */
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            0,
            0,
        )
        .expect("ok");
    }
    assert_eq!(
        b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
        "standard ops (Add/Src/OutReverse) must not hit the unsupported gate",
    );
}

/// Stage 3f.12: gradient src is no longer a "drop" — it
/// collapses to a SolidFill of the first stop's premultiplied
/// colour (real LUT sampling is still post-3f.5 work). The
/// composite_glyphs path now accepts gradient sources; the
/// `composite_glyphs_dropped_unsupported` counter stays at 0.
/// Cairo glyph rendering with gradient bg/fg therefore paints
/// (with the gradient flattened to its start colour) rather
/// than dropping entirely.
#[test]
fn composite_glyphs_gradient_source_collapses_to_solidfill() {
    let mut b = KmsBackend::for_tests();
    let (_unused_solidfill, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
    // Minimal valid linear-gradient wire body: pad(4) +
    // p1(8) + p2(8) + n_stops=1(4) + stop_pos(4) + stop_color(8).
    let mut grad_body: Vec<u8> = Vec::new();
    grad_body.extend_from_slice(&0_u32.to_le_bytes()); // request pad (skipped)
    grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
    grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
    grad_body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
    grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
    grad_body.extend_from_slice(&1_u32.to_le_bytes()); // n_stops
    grad_body.extend_from_slice(&0_i32.to_le_bytes()); // pos
    grad_body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]); // colour
    let grad_pic = b
        .render_create_linear_gradient(None, &grad_body)
        .expect("gradient")
        .expect("Some")
        .as_raw();
    b.render_composite_glyphs(
        None,
        23,
        3, /* Over */
        grad_pic,
        0xDEAD,
        0,
        gs_xid,
        0,
        0,
        &[1u8, 0, 0, 0, 0, 0, 0, 0],
        0,
        0,
    )
    .expect("ok");
    assert_eq!(
        b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
        "gradient src must collapse to SolidFill (not drop)",
    );
}

/// Per plan §3d items-parse spec: the items stream's inline
/// `0xFF 0 0 0 new_gs_xid` element rotates the active glyphset
/// for subsequent glyph lookups.
///
/// **This test used to assert nothing of the sort.** Its only
/// gate was `composite_glyphs_dropped_unsupported == 0`, which
/// holds whether the inline element is honoured or silently
/// skipped — a parse that ignored it would resolve one glyph
/// instead of two and still pass. (Reported honestly by #137 step
/// 4b, whose own tests do catch it; fixed here rather than left
/// as a name that promises coverage it does not have.)
///
/// It now drives `parse_composite_glyph_items` — the seam step 4b
/// factored out — and asserts the resolved glyph SEQUENCE:
/// glyphset, id and pen position, in request order. The negative
/// control at the end is what gives it teeth: point the inline
/// change at an unknown xid and the second glyph becomes a MISS,
/// because the initial glyphset does not contain its id.
#[test]
fn composite_glyphs_inline_glyphset_change_parsed() {
    use crate::kms::{
        core::{GlyphSetFormat, GlyphSetState, StoredGlyph},
        render::backend::parse_composite_glyph_items,
    };

    let mut b = KmsBackend::for_tests();
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some")
        .as_raw();
    // GlyphSet A: codepoint 0x10 → 0xAA pixels.
    // GlyphSet B: codepoint 0x20 → 0xBB pixels.
    let mut mk_gs = |code: u32, byte: u8| {
        let mut glyphs = HashMap::new();
        glyphs.insert(
            code,
            StoredGlyph {
                width: 1,
                height: 1,
                x: 0,
                y: 0,
                x_off: 1,
                y_off: 0,
                pixels: vec![byte],
                format: GlyphSetFormat::A8,
            },
        );
        let xid = b.core.next_host_xid();
        b.core.glyphsets.insert(
            xid,
            GlyphSetState {
                format: GlyphSetFormat::A8,
                glyphs,
            },
        );
        xid
    };
    let gs_a = mk_gs(0x10, 0xAA);
    let gs_b = mk_gs(0x20, 0xBB);
    // Need a dst Drawable picture — create a stub one (lookup
    // will fail since the underlying drawable xid isn't in
    // the store, so the engine call short-circuits before
    // anything reaches Vk, but the parser still walks).
    use yserver_core::backend::{AnyHandle, PixmapHandle};
    let dst_drawable =
        AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));
    let dst_pic = b
        .render_create_picture(None, dst_drawable, 0, 0, &[])
        .expect("dst_picture")
        .expect("Some")
        .as_raw();
    // Items stream: 1 glyph 0x10 from gs_a (initial), inline
    // glyphset-change to gs_b, then 1 glyph 0x20 from gs_b.
    // Element layout: count(u8) pad pad pad dx(i16) dy(i16) ids...
    let mut items: Vec<u8> = Vec::new();
    // Element 1: 1 glyph @ (0,0).
    items.extend_from_slice(&[1u8, 0, 0, 0, 0, 0, 0, 0]);
    items.extend_from_slice(&[0x10, 0, 0, 0]); // padded ids
    // Element 2: glyphset change.
    items.push(255);
    items.extend_from_slice(&[0u8, 0, 0]);
    items.extend_from_slice(&gs_b.to_le_bytes());
    // Element 3: 1 glyph @ (+1,0).
    items.extend_from_slice(&[1u8, 0, 0, 0, 1, 0, 0, 0]);
    items.extend_from_slice(&[0x20, 0, 0, 0]);

    b.render_composite_glyphs(
        None, 23, 3, /* Over */
        src_pic, dst_pic, 0, gs_a, 0, 0, &items, 0, 0,
    )
    .expect("ok");
    // Op + source were Over + SolidFill, so the unsupported
    // counter must NOT have fired. (dst resolution fails — no
    // Drawable backing for 0x4242_4242 in the store — so the
    // engine is never reached; engine reachability is covered by
    // the Vk-backed acceptance tests.)
    assert_eq!(
        b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
        "Over + SolidFill must not hit the unsupported gate",
    );

    // ── what the test's name actually claims ──
    //
    // Element 1 draws 0x10 from gs_a at pen 0; the glyph advances
    // the pen by its x_off of 1; element 3's dx of 1 takes it to
    // 2, where 0x20 comes from gs_b. Two glyphs, TWO glyphsets,
    // in request order.
    let parsed = parse_composite_glyph_items(&b.core.glyphsets, 23, gs_a, 0, 0, &items);
    assert_eq!(
        parsed
            .glyphs
            .iter()
            .map(|g| (g.gs_xid, g.glyph_id, g.dst_x))
            .collect::<Vec<_>>(),
        vec![(gs_a, 0x10, 0), (gs_b, 0x20, 2)],
        "the inline `count == 255` element must rotate the active glyphset \
             for every later glyph, in request order",
    );
    assert_eq!(
        parsed.missing, 0,
        "both ids must resolve in their own glyphset"
    );

    // Teeth: point the inline change at an xid no glyphset holds.
    // The active glyphset then stays gs_a, which has no 0x20, so
    // the second glyph is a MISS — i.e. this stream really does
    // depend on the inline element being honoured, and a parse
    // that skipped it would produce exactly this.
    let mut ignored = items.clone();
    let change_at = 12; // element 1 is 8 + 4 bytes
    assert_eq!(
        ignored[change_at], 255,
        "fixture: the change element is here"
    );
    ignored[change_at + 4..change_at + 8].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
    let parsed_ignored = parse_composite_glyph_items(&b.core.glyphsets, 23, gs_a, 0, 0, &ignored);
    assert_eq!(
        parsed_ignored
            .glyphs
            .iter()
            .map(|g| (g.gs_xid, g.glyph_id))
            .collect::<Vec<_>>(),
        vec![(gs_a, 0x10)],
        "with the change unresolvable, only the first glyph can be found",
    );
    assert_eq!(parsed_ignored.missing, 1, "0x20 is not in gs_a");
}
