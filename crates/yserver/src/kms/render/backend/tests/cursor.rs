use super::*;

/// Stage 3f.4 close: cursor-creation calls mint valid handles
/// without logging gaps. `create_cursor`, `create_glyph_cursor`,
/// `render_create_cursor`, `define_cursor`, and
/// `replace_cursor` all return `Ok` with no
/// `log_render_gap` noise. Pixel rasterisation + scene blit is
/// Stage 4 (cursor scene-layer work); 3f.4's job is to silence
/// the pre-Stage-4 stub warnings that were misleading
/// real-app smoke matrix triage.
#[test]
fn cursor_paths_do_not_log_gaps() {
    use yserver_core::backend::{FontHandle, PictureHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0001).unwrap();
    let font = FontHandle::from_raw(0x1234_0002).unwrap();
    let pic = PictureHandle::from_raw(0x1234_0003).unwrap();

    let c1 = b
        .create_cursor(None, pix, None, (0xFF00, 0, 0), (0, 0, 0xFF00), 4, 4)
        .expect("create_cursor");
    assert!(c1.as_raw() != 0);

    let c2 = b
        .create_glyph_cursor(None, font, None, b'X' as u16, 0, (0, 0, 0), (0, 0, 0))
        .expect("create_glyph_cursor");
    assert!(c2.as_raw() != 0);

    // Stage 5 Phase A: render_create_cursor returns None when
    // the picture xid isn't registered as a Drawable picture
    // (no rasterisation source); the contract is "don't log a
    // gap", not "always mint a handle".
    let _ = b
        .render_create_cursor(None, pic, 0, 0)
        .expect("render_create_cursor");

    b.define_cursor(None, 0xABCD_EF01, c1.as_raw())
        .expect("define_cursor");
    b.replace_cursor(None, c1.as_raw(), c2.as_raw())
        .expect("replace_cursor");

    let gaps = b.logged_gaps.borrow();
    for g in [
        "create_cursor",
        "create_glyph_cursor",
        "render_create_cursor",
        "define_cursor",
        "replace_cursor",
    ] {
        assert!(
            !gaps.contains(g),
            "{g} must not log a gap post-3f.4 (cursor scene blit is Stage 4)"
        );
    }
}

#[test]
fn recolor_cursor_updates_monochrome_records_and_ignores_argb() {
    use crate::kms::render::cursor::{CursorColorRole, color_roles_to_bgra};

    let mut b = KmsBackend::for_tests();
    let mono = 0x1234_1001;
    let roles = vec![
        CursorColorRole::Transparent,
        CursorColorRole::Foreground,
        CursorColorRole::Background,
    ];
    let original = color_roles_to_bgra(&roles, (0x1111, 0x2222, 0x3333), (0, 0, 0));
    b.insert_monochrome_cursor_record(mono, 3, 1, 0, 0, original, roles);
    let old_version = b.cursor_records[&mono].version;

    b.recolor_cursor(None, mono, (0xff00, 0, 0), (0, 0, 0xff00))
        .expect("recolor monochrome cursor");

    let recolored = &b.cursor_records[&mono];
    assert!(recolored.version > old_version);
    assert_eq!(&recolored.bgra_bytes[0..4], &[0, 0, 0, 0]);
    assert_eq!(&recolored.bgra_bytes[4..8], &[0, 0, 0xff, 0xff]);
    assert_eq!(&recolored.bgra_bytes[8..12], &[0xff, 0, 0, 0xff]);

    let argb = 0x1234_1002;
    b.insert_cursor_record(argb, 1, 1, 0, 0, vec![1, 2, 3, 4]);
    let argb_before = b.cursor_records[&argb].clone();
    b.recolor_cursor(None, argb, (0xffff, 0, 0), (0, 0, 0xffff))
        .expect("ARGB recolor is an intentional no-op");
    let argb_after = &b.cursor_records[&argb];
    assert_eq!(argb_after.version, argb_before.version);
    assert_eq!(argb_after.bgra_bytes, argb_before.bgra_bytes);
}

/// Stage 5 Phase A: define_cursor stores the cursor on the
/// window's geometry slot and (when the window is the root
/// container) updates `KmsCore.active_cursor` so unbound child
/// windows inherit the new sprite via the parent-chain walk.
#[test]
fn define_cursor_records_per_window_and_root_sticky() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0010).unwrap();
    let c = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0xFFFF, 0), 0, 0)
        .expect("create_cursor");

    // DefineCursor on the root container — sticky fallback.
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, c.as_raw())
        .expect("define_cursor root");
    assert_eq!(b.core.active_cursor, Some(c.as_raw()));

    // DefineCursor on a non-root window — stored on geom only,
    // does NOT touch `active_cursor`.
    let w: u32 = 0xABCD_0001;
    let rank = b.alloc_window_stack_rank();
    b.windows.insert(
        w,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0xFFFF, 0, 0), 0, 0)
        .expect("create_cursor 2");
    b.define_cursor(None, w, c2.as_raw())
        .expect("define_cursor non-root");
    assert_eq!(
        b.core.active_cursor,
        Some(c.as_raw()),
        "non-root must not touch active_cursor"
    );
    assert_eq!(b.windows.get(&w).and_then(|g| g.cursor), Some(c2.as_raw()));

    // `define_cursor(_, 0)` (X11 None) clears the per-window slot.
    b.define_cursor(None, w, 0).expect("define_cursor clear");
    assert_eq!(b.windows.get(&w).and_then(|g| g.cursor), None);
}

fn insert_test_window(b: &mut KmsBackend, w: u32) {
    let rank = b.alloc_window_stack_rank();
    b.windows.insert(
        w,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
}

/// #196: FreeCursor drops only the XID's reference (Xorg `FreeCursor`
/// decrements `refcnt`): a window still using the cursor keeps it, and
/// it goes when the window's cursor changes (`dix/window.c:1559`).
/// `free_cursor` used to be a no-op, so every cursor ever created —
/// and its sprite pixmap — lived for the session.
#[test]
fn freed_cursor_lives_until_its_window_stops_using_it() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let c = test_cursor(&mut b);
    let w = 0xABCD_0101;
    insert_test_window(&mut b, w);
    b.define_cursor(None, w, c).expect("define");
    b.free_cursor(None, c).expect("free");
    assert!(
        b.cursor_records.contains_key(&c),
        "the window still holds it"
    );
    b.define_cursor(None, w, 0).expect("undefine");
    assert!(!b.cursor_records.contains_key(&c));
    assert!(b.released_cursors.is_empty());
}

#[test]
fn freed_cursor_goes_with_the_window_that_used_it() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let c = test_cursor(&mut b);
    let w = 0xABCD_0102;
    insert_test_window(&mut b, w);
    b.define_cursor(None, w, c).expect("define");
    b.free_cursor(None, c).expect("free");
    b.destroy_subwindow(None, w).expect("destroy");
    assert!(!b.cursor_records.contains_key(&c), "dix/window.c:968");
}

#[test]
fn unreferenced_cursor_is_destroyed_at_free_cursor() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let c = test_cursor(&mut b);
    b.free_cursor(None, c).expect("free");
    assert!(!b.cursor_records.contains_key(&c));
}

/// The grab holds a ref for its duration (`dix/grabs.c:243`/`:261`).
#[test]
fn freed_grab_cursor_lives_until_the_grab_ends() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let c = test_cursor(&mut b);
    b.set_grab_cursor(None, Some(c)).expect("grab");
    b.free_cursor(None, c).expect("free");
    assert!(b.cursor_records.contains_key(&c));
    assert_eq!(b.effective_cursor_xid, Some(c), "still displayed");
    b.set_grab_cursor(None, None).expect("ungrab");
    assert!(!b.cursor_records.contains_key(&c));
}

/// The root's cursor (our sticky default) holds it until replaced.
#[test]
fn freed_root_cursor_lives_until_the_root_cursor_changes() {
    use yserver_core::backend::Backend;
    let mut b = KmsBackend::for_tests();
    let (c, c2) = (test_cursor(&mut b), test_cursor(&mut b));
    let root = b.core.window_id;
    b.define_cursor(None, root, c).expect("define root");
    b.free_cursor(None, c).expect("free");
    assert!(b.cursor_records.contains_key(&c));
    b.define_cursor(None, root, c2).expect("redefine root");
    assert!(!b.cursor_records.contains_key(&c));
    assert!(b.cursor_records.contains_key(&c2));
}

/// An animated cursor refs its frames (`render/animcur.c:360`), so a
/// client may free them right after CreateAnimCursor (libXcursor does);
/// they go with the animated cursor (`animcur.c:247`).
#[test]
fn animated_cursor_keeps_its_freed_frames_alive() {
    use yserver_core::backend::{Backend, CursorHandle};
    let mut b = KmsBackend::for_tests();
    let (c1, c2) = (test_cursor(&mut b), test_cursor(&mut b));
    let anim = b
        .create_anim_cursor(
            None,
            &[
                (CursorHandle::from_raw(c1).unwrap(), 50),
                (CursorHandle::from_raw(c2).unwrap(), 50),
            ],
        )
        .expect("anim")
        .expect("KMS animates")
        .as_raw();
    b.free_cursor(None, c1).expect("free c1");
    b.free_cursor(None, c2).expect("free c2");
    assert!(b.cursor_records.contains_key(&c1) && b.cursor_records.contains_key(&c2));
    b.free_cursor(None, anim).expect("free anim");
    for xid in [c1, c2, anim] {
        assert!(!b.cursor_records.contains_key(&xid), "{xid:#x}");
    }
    assert!(b.anim_cursor_records.is_empty());
}

/// #196 with real Vk: destroying a cursor releases its sprite pixmap.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn freed_cursor_releases_its_sprite_pixmap() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let c = test_cursor(&mut b);
    let sprite = *b.cursor_pixmaps.get(&c).expect("sprite allocated");
    let w = 0xABCD_0103;
    insert_test_window(&mut b, w);
    b.define_cursor(None, w, c).expect("define");
    b.free_cursor(None, c).expect("free");
    assert!(b.store.get(sprite).is_some(), "in use: sprite kept");
    b.define_cursor(None, w, 0).expect("undefine");
    assert!(!b.cursor_records.contains_key(&c), "cursor destroyed");
    // The sprite upload is still in the open frame: submit it, then retire.
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
    assert!(b.store.get(sprite).is_none(), "sprite pixmap released");
}

/// A pixmap freed while its clear is in flight is parked; an otherwise
/// idle server must still wake to release it once the fence signals.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn idle_server_wakes_to_release_a_pixmap_freed_in_flight() {
    use yserver_core::backend::Backend;
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Display dark: no compose deadline, so any wake is the retire poll.
    b.kms_outputs_active = false;
    let xid = b.create_pixmap(None, 32, 61, 59).expect("pixmap").as_raw();
    let id = b.store.lookup(xid).expect("pixmap in store");
    b.free_pixmap(None, xid).expect("free pixmap");
    assert_eq!(b.store.pending_retire_count(), 1, "clear still in flight");
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
    let wake = b.next_wakeup().expect("parked pixmap schedules a wake");
    assert!(
        wake <= std::time::Instant::now()
            + crate::kms::render::backend::PENDING_RETIRE_POLL_INTERVAL
    );
    b.platform.wait_idle_bounded();
    b.before_block();
    assert!(b.store.get(id).is_none(), "freed pixmap released on wake");
    assert_eq!(b.store.pending_retire_count(), 0);
    assert!(b.next_wakeup().is_none(), "nothing left to wake for");
}

/// A GrabPointer cursor is the top-priority sprite: it overrides the
/// per-window cursor for the grab's duration and reverts on ungrab
/// (Xorg ActivatePointerGrab / DeactivatePointerGrab). Regression
/// guard for #90 — ImageMagick `import` grabs with a crosshair that
/// was stored on the grab record but never applied to the sprite.
#[test]
fn grab_cursor_override_wins_and_reverts() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0030).unwrap();
    let win_cur = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("create window cursor");
    let grab_cur = b
        .create_cursor(None, pix, None, (0, 0, 0xFFFF), (0, 0, 0), 0, 0)
        .expect("create grab cursor");

    // A window with its own defined cursor, sitting under the pointer.
    let w: u32 = 0xBEEF_0001;
    let rank = b.alloc_window_stack_rank();
    b.windows.insert(
        w,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.core.prev_pointer_window = Some(w);
    b.define_cursor(None, w, win_cur.as_raw())
        .expect("define window cursor");

    // Baseline: the window's own cursor is effective (define_cursor
    // ran refresh_effective_cursor for the pointer window).
    assert_eq!(b.effective_cursor_xid, Some(win_cur.as_raw()));

    // GrabPointer with a cursor → override wins over the window
    // cursor (refresh swaps the sprite).
    b.set_grab_cursor(None, Some(grab_cur.as_raw()))
        .expect("set grab cursor");
    assert_eq!(b.effective_cursor_xid, Some(grab_cur.as_raw()));

    // UngrabPointer (None) → reverts to the window's cursor.
    b.set_grab_cursor(None, None).expect("clear grab cursor");
    assert_eq!(b.effective_cursor_xid, Some(win_cur.as_raw()));
}

fn cursor_test_window(b: &mut KmsBackend, w: u32) {
    let rank = b.alloc_window_stack_rank();
    b.windows.insert(
        w,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
}

/// XFIXES CursorNotify source: the backend reports each switch of the
/// effective cursor once, with the serial `GetCursorImage` reports, and
/// nothing when the cursor stays the same (Xvfb: re-defining the same
/// cursor sends no event). Hiding does not count as a change (Xvfb:
/// HideCursor sends no event, DefineCursor while hidden does).
#[test]
fn effective_cursor_changes_are_reported_once_and_survive_hiding() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0040).unwrap();
    let a = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("cursor a");
    let c = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("cursor c");
    let w: u32 = 0xBEEF_0002;
    cursor_test_window(&mut b, w);
    b.core.prev_pointer_window = Some(w);
    let _ = b.take_displayed_cursor_change();

    b.define_cursor(None, w, a.as_raw()).expect("define a");
    let change = b.take_displayed_cursor_change().expect("a reported");
    assert_eq!(change.host_xid, a.as_raw());
    let image = b.get_active_cursor_image().expect("image");
    assert_eq!(
        change.serial, image.serial,
        "notify serial == GetCursorImage serial"
    );
    assert_eq!(image.host_xid, a.as_raw());

    b.define_cursor(None, w, a.as_raw())
        .expect("define a again");
    assert_eq!(
        b.take_displayed_cursor_change(),
        None,
        "same cursor: no report"
    );

    b.set_cursor_hidden(true);
    assert!(b.cursor_hidden);
    assert_eq!(
        b.take_displayed_cursor_change(),
        None,
        "hiding is not a change"
    );
    b.define_cursor(None, w, c.as_raw())
        .expect("define c while hidden");
    assert_eq!(
        b.take_displayed_cursor_change().map(|d| d.host_xid),
        Some(c.as_raw()),
        "a change while hidden is still reported",
    );
    assert_eq!(b.effective_cursor_xid, Some(c.as_raw()));
    b.set_cursor_hidden(false);
    assert!(!b.cursor_hidden);
    assert_eq!(
        b.take_displayed_cursor_change(),
        None,
        "showing is not a change"
    );
}

/// XFIXES ChangeCursor (Xorg `ReplaceCursor`): every window slot, the
/// sticky root default and the grab override naming the old cursor move
/// to the new one, and the sprite follows.
#[test]
fn replace_cursor_rewrites_every_reference() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0050).unwrap();
    let old = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("old");
    let new = b
        .create_cursor(None, pix, None, (0, 0, 0xFFFF), (0, 0, 0), 0, 0)
        .expect("new");
    let other = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("other");
    let (w1, w2) = (0xBEEF_0003, 0xBEEF_0004);
    cursor_test_window(&mut b, w1);
    cursor_test_window(&mut b, w2);
    b.core.prev_pointer_window = Some(w1);
    b.define_cursor(None, w1, old.as_raw()).expect("w1 old");
    b.define_cursor(None, w2, other.as_raw()).expect("w2 other");
    let root = b.core.window_id;
    b.define_cursor(None, root, old.as_raw()).expect("root old");
    b.grab_cursor_override = Some(old.as_raw());
    let _ = b.take_displayed_cursor_change();

    b.replace_cursor(None, old.as_raw(), new.as_raw())
        .expect("replace");
    assert_eq!(b.windows[&w1].cursor, Some(new.as_raw()));
    assert_eq!(b.windows[&w2].cursor, Some(other.as_raw()), "untouched");
    assert_eq!(b.core.active_cursor, Some(new.as_raw()));
    assert_eq!(b.grab_cursor_override, Some(new.as_raw()));
    assert_eq!(b.effective_cursor_xid, Some(new.as_raw()));
    assert_eq!(
        b.take_displayed_cursor_change().map(|d| d.host_xid),
        Some(new.as_raw())
    );
}

/// Effective-cursor walk: a child without its own cursor inherits
/// from its parent; a fresh root cursor (DefineCursor on root)
/// becomes the fallback when no chain entry binds one.
#[test]
fn effective_cursor_walks_parent_chain() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0011).unwrap();
    let root_cur = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("create_cursor");
    let parent_cur = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("create_cursor parent");
    // Wire: root → parent → child. Parent has its own cursor;
    // child inherits.
    let root_host = b.core.window_id;
    let parent: u32 = 0xDEAD_0001;
    let child: u32 = 0xDEAD_0002;
    let rank_p = b.alloc_window_stack_rank();
    let rank_c = b.alloc_window_stack_rank();
    b.windows.insert(
        parent,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(root_host),
            stack_rank: rank_p,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.windows.insert(
        child,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: Some(parent),
            stack_rank: rank_c,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    // DefineCursor on root + parent.
    b.define_cursor(None, root_host, root_cur.as_raw())
        .expect("root");
    b.define_cursor(None, parent, parent_cur.as_raw())
        .expect("parent");
    // Child inherits parent's cursor (parent has its own bound).
    assert_eq!(
        b.effective_cursor_walking_chain(child),
        Some(parent_cur.as_raw())
    );
    // Parent itself reports its own cursor.
    assert_eq!(
        b.effective_cursor_walking_chain(parent),
        Some(parent_cur.as_raw())
    );
    // Window unknown to windows → falls back to active_cursor
    // (root's DefineCursor).
    assert_eq!(
        b.effective_cursor_walking_chain(0xFFFF_FFFF),
        Some(root_cur.as_raw())
    );
}

/// With no root cursor set the screen shows Xorg's: `X_cursor` from the
/// cursor font, black on white. Size, hotspot and the FNV-1a hash of
/// the ARGB pixels as XFixesGetCursorImage reports them on Xorg
/// (tools/vng-scenarios/goldens/cursor.txt, "bare root").
#[test]
fn default_cursor_is_xorg_root_cursor() {
    let b = KmsBackend::for_tests();
    let record = b
        .cursor_records
        .get(&b.default_cursor_xid.expect("default cursor"))
        .expect("default record");
    assert_eq!(
        (record.width, record.height, record.hot_x, record.hot_y),
        (16, 16, 7, 7)
    );
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for px in record.bgra_bytes.chunks_exact(4) {
        h ^= u64::from(u32::from_le_bytes([px[0], px[1], px[2], px[3]]));
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    assert_eq!(h, 0x1ae7_d436_690f_ff69);
}

/// CursorRecord versions are monotonically increasing — each
/// `create_cursor` allocates a fresh version, and the boot-time
/// default sits at version 1.
#[test]
fn cursor_record_versions_monotonic() {
    use std::sync::Arc;
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let default_xid = b.default_cursor_xid.expect("default cursor xid set");
    let v0 = b
        .cursor_records
        .get(&default_xid)
        .expect("default record")
        .version;

    let pix = PixmapHandle::from_raw(0x1234_0020).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("create_cursor");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("create_cursor 2");

    let v1 = b.cursor_records.get(&c1.as_raw()).unwrap().version;
    let v2 = b.cursor_records.get(&c2.as_raw()).unwrap().version;
    assert!(v0 < v1, "v0={v0} v1={v1}");
    assert!(v1 < v2, "v1={v1} v2={v2}");

    // Captured Arc reference observes its original bytes even
    // after later allocations.
    let captured: Arc<crate::kms::render::cursor::CursorRecord> =
        Arc::clone(b.cursor_records.get(&c1.as_raw()).unwrap());
    let snapshot = captured.bgra_bytes.clone();
    let _ = b
        .create_cursor(None, pix, None, (0, 0, 0xFFFF), (0, 0, 0), 0, 0)
        .expect("create_cursor 3");
    assert_eq!(captured.bgra_bytes, snapshot);
}

/// CreateAnimCursor snapshots frames at creation: maps gain an
/// entry aliasing frame 0; the AnimCursorRecord holds Arc'd frame
/// records + clamped delays.
#[test]
fn create_anim_cursor_snapshots_frames() {
    use std::time::Duration;
    use yserver_core::backend::{Backend, CursorHandle, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0030).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 1, 2)
        .expect("c1");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 3, 4)
        .expect("c2");

    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 0)])
        .expect("create_anim_cursor")
        .expect("KMS animates");

    let rec = b
        .anim_cursor_records
        .get(&anim.as_raw())
        .expect("anim record");
    assert_eq!(rec.frames.len(), 2);
    assert_eq!(rec.frames[0].delay, Duration::from_millis(50));
    // Delay 0 clamps to 16ms (spec: explicit Xorg deviation).
    assert_eq!(rec.frames[1].delay, Duration::from_millis(16));
    // The anim handle aliases frame 0 in the canonical map.
    assert_eq!(
        b.cursor_records.get(&anim.as_raw()).unwrap().version,
        b.cursor_records.get(&c1.as_raw()).unwrap().version,
    );
    // Unknown sub-cursor handle → error, no partial state.
    let bogus = CursorHandle::from_raw(0xDEAD_BEEF).unwrap();
    let before = b.anim_cursor_records.len();
    let recs_before = b.cursor_records.len();
    let pix_before = b.cursor_pixmaps.len();
    assert!(
        b.create_anim_cursor(None, &[(c1, 10), (bogus, 10)])
            .is_err()
    );
    assert_eq!(b.anim_cursor_records.len(), before);
    assert_eq!(b.cursor_records.len(), recs_before);
    assert_eq!(b.cursor_pixmaps.len(), pix_before);
}

/// Effective-cursor change arms/clears the animation; re-resolving
/// to the same cursor preserves the running frame index.
#[test]
fn effective_cursor_arms_and_clears_animation() {
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0040).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("c1");
    let c2 = b
        .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
        .expect("c2");
    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
        .expect("anim")
        .expect("KMS animates");

    // Purely static cursor → anim state stays cleared.
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, c1.as_raw())
        .expect("static before anim");
    assert!(
        b.active_cursor_anim.is_none(),
        "static cursor must not arm animation"
    );

    // Bind the anim cursor on root → it becomes effective and arms.
    b.define_cursor(None, root_host, anim.as_raw())
        .expect("define anim");
    let st = b.active_cursor_anim.as_ref().expect("armed");
    assert_eq!(st.handle, anim.as_raw());
    assert_eq!(st.frame, 0);
    // Arming mints a fresh version for frame 0 (monotonic serial).
    let v_frame0 = b.cursor_records.get(&c1.as_raw()).unwrap().version;
    let v_anim = b.cursor_records.get(&anim.as_raw()).unwrap().version;
    assert!(
        v_anim > v_frame0,
        "armed version must be minted, not aliased"
    );

    // Pretend the animation advanced, then re-resolve to the SAME
    // cursor: frame index must be preserved (no restart).
    b.active_cursor_anim.as_mut().unwrap().frame = 1;
    b.refresh_effective_cursor();
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, 1);

    // Switch to a static cursor → animation cleared.
    b.define_cursor(None, root_host, c1.as_raw())
        .expect("define static");
    assert!(b.active_cursor_anim.is_none());

    // Switch back → restarts at frame 0.
    b.define_cursor(None, root_host, anim.as_raw())
        .expect("re-define anim");
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, 0);
}

/// Frame tick: advances mod n, re-arms relative, mints strictly
/// increasing versions across a full wraparound (XFixes serial
/// contract — naive Arc-swapping would repeat v1,v2,v1).
#[test]
fn anim_tick_advances_wraps_and_stays_monotonic() {
    use std::time::{Duration, Instant};
    use yserver_core::backend::{Backend, CursorHandle};

    let mut b = KmsBackend::for_tests();
    // Insert records directly with DISTINCT bytes per frame —
    // `create_cursor` on an unreadable test pixmap degenerates to
    // identical 1×1 transparent bytes, making the bytes assert
    // below vacuous.
    let c1_xid = b.core.next_host_xid();
    b.insert_cursor_record(c1_xid, 1, 1, 0, 0, vec![0xAA, 0x00, 0x00, 0xFF]);
    let c1 = CursorHandle::from_raw(c1_xid).unwrap();
    let c2_xid = b.core.next_host_xid();
    b.insert_cursor_record(c2_xid, 1, 1, 0, 0, vec![0x00, 0xBB, 0x00, 0xFF]);
    let c2 = CursorHandle::from_raw(c2_xid).unwrap();
    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
        .expect("anim")
        .expect("KMS animates");
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw())
        .expect("define");

    let mut last_version = b.cursor_records.get(&anim.as_raw()).unwrap().version;
    let mut expected_frame = 0usize;
    // 5 ticks over 2 frames = two full wraparounds.
    for i in 0..5 {
        // Force the deadline into the past, then tick.
        b.active_cursor_anim.as_mut().unwrap().next_frame =
            Instant::now() - Duration::from_millis(1);
        b.tick_cursor_animation();
        expected_frame = (expected_frame + 1) % 2;
        let st = b.active_cursor_anim.as_ref().expect("still armed");
        assert_eq!(st.frame, expected_frame, "tick {i}");
        assert!(st.next_frame > Instant::now() - Duration::from_millis(1));
        let v = b.cursor_records.get(&anim.as_raw()).unwrap().version;
        assert!(v > last_version, "tick {i}: version {v} !> {last_version}");
        last_version = v;
        // The canonical record now carries the frame's bytes.
        let frame_rec =
            &b.anim_cursor_records.get(&anim.as_raw()).unwrap().frames[expected_frame].record;
        assert_eq!(
            b.cursor_records.get(&anim.as_raw()).unwrap().bgra_bytes,
            frame_rec.bgra_bytes,
        );
    }
    // Tick before the deadline → no advance.
    let frame_before = b.active_cursor_anim.as_ref().unwrap().frame;
    b.tick_cursor_animation();
    assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, frame_before);
}

/// next_wakeup reports the anim deadline only while outputs are
/// active and scanout is allowed (EINVAL-storm discipline).
#[test]
fn anim_deadline_gated_on_outputs_active() {
    use crate::vt::state::VtState;
    use std::time::{Duration, Instant};
    use yserver_core::backend::{Backend, PixmapHandle};

    let mut b = KmsBackend::for_tests();
    let pix = PixmapHandle::from_raw(0x1234_0060).unwrap();
    let c1 = b
        .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("c1");
    let anim = b
        .create_anim_cursor(None, &[(c1, 50)])
        .expect("anim")
        .expect("KMS animates");
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw())
        .expect("define");
    let deadline = b.active_cursor_anim.as_ref().unwrap().next_frame;

    let wake = b.next_wakeup().expect("deadline reported");
    assert!(wake <= deadline);

    // DPMS off → deadline not reported, tick is a no-op.
    b.kms_outputs_active = false;
    let frame = b.active_cursor_anim.as_ref().unwrap().frame;
    b.active_cursor_anim.as_mut().unwrap().next_frame = Instant::now() - Duration::from_millis(1);
    assert!(
        b.next_wakeup().is_none_or(|w| w > Instant::now()),
        "stale anim deadline must not be reported while outputs are off",
    );
    b.tick_cursor_animation();
    assert_eq!(
        b.active_cursor_anim.as_ref().unwrap().frame,
        frame,
        "tick must not advance while outputs are off",
    );

    // Outputs back on with the deadline in the past → exactly one
    // immediate advance (spec: no fast-forward through missed frames).
    b.kms_outputs_active = true;
    b.tick_cursor_animation();
    assert_eq!(
        b.active_cursor_anim.as_ref().unwrap().frame,
        frame,
        "1-frame anim wraps to same index"
    );
    assert!(
        b.active_cursor_anim.as_ref().unwrap().next_frame > Instant::now(),
        "re-armed from now: next_frame must be a future instant after DPMS restore tick",
    );

    // VT-away / master-drop uses the same scheduler suppression.
    b.vt_state = VtState::Suspended;
    b.active_cursor_anim.as_mut().unwrap().next_frame = Instant::now() - Duration::from_millis(1);
    assert!(
        b.next_wakeup().is_none_or(|w| w > Instant::now()),
        "stale anim deadline must not be reported while scanout is disallowed",
    );
    b.tick_cursor_animation();
    assert_eq!(
        b.active_cursor_anim.as_ref().unwrap().frame,
        frame,
        "tick must not advance while scanout is disallowed",
    );
}

/// XFixes GetCursorImage tracks the animation: bytes follow the
/// current frame, serial strictly increases across a wraparound.
#[test]
fn xfixes_cursor_image_follows_animation_frames() {
    use std::time::{Duration, Instant};
    use yserver_core::backend::{Backend, CursorHandle};

    let mut b = KmsBackend::for_tests();
    // Insert records directly with DISTINCT bytes per frame —
    // `create_cursor` on an unreadable test pixmap degenerates to
    // identical 1×1 transparent bytes, making the bytes assert
    // below vacuous.
    let c1_xid = b.core.next_host_xid();
    b.insert_cursor_record(c1_xid, 1, 1, 0, 0, vec![0xAA, 0x00, 0x00, 0xFF]);
    let c1 = CursorHandle::from_raw(c1_xid).unwrap();
    let c2_xid = b.core.next_host_xid();
    b.insert_cursor_record(c2_xid, 1, 1, 0, 0, vec![0x00, 0xBB, 0x00, 0xFF]);
    let c2 = CursorHandle::from_raw(c2_xid).unwrap();
    let anim = b
        .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
        .expect("anim")
        .expect("KMS animates");
    let root_host = b.core.window_id;
    b.define_cursor(None, root_host, anim.as_raw())
        .expect("define");

    let mut last_serial = b.get_active_cursor_image().expect("image").serial;
    for _ in 0..4 {
        b.active_cursor_anim.as_mut().unwrap().next_frame =
            Instant::now() - Duration::from_millis(1);
        b.tick_cursor_animation();
        let img = b.get_active_cursor_image().expect("image");
        assert!(img.serial > last_serial, "serial must strictly increase");
        last_serial = img.serial;
        let frame_idx = b.active_cursor_anim.as_ref().unwrap().frame;
        let frame_rec =
            &b.anim_cursor_records.get(&anim.as_raw()).unwrap().frames[frame_idx].record;
        assert_eq!(*img.bgra_bytes, frame_rec.bgra_bytes);
    }
}
