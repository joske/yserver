mod gc;
mod tree;
mod window;
use super::*;
use proptest::prelude::*;
use yserver_protocol::x11::{ChangeWindowAttributesRequest, ClientId, CreateWindowRequest};

fn make_window(table: &mut ResourceTable, id: u32) {
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(id),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
}

fn make_top_level_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
    make_window(table, id);
    table.windows.get_mut(&id).unwrap().host_xid =
        Some(crate::backend::WindowHandle::from_raw_for_test(host_xid));
}

fn make_child(table: &mut ResourceTable, id: u32, parent: u32, x: i16, y: i16) {
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(id),
            parent: ResourceId(parent),
            x,
            y,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
}

/// Regression for the "leaked XID → BadIDChoice after recycling"
/// family. `xid_in_use` walks every core resource map; for the
/// IdAllocator recycle path to be sound, EVERY map it checks must
/// be drained by `remove_non_window_resources_owned_by` (windows
/// go through `destroy_window` separately).
///
/// Populates one entry per map for `ClientId(K)`, runs the
/// disconnect cleanup, and asserts each test XID reads
/// `xid_in_use == false`. Adding a new map to `xid_in_use` should
/// be accompanied by a new line in this test + cleanup logic in
/// `remove_non_window_resources_owned_by`; without that the test
/// stays passing for the existing maps but a new leak ships silent.
#[test]
fn resource_counts_by_owner_tallies_per_type_and_skips_zero() {
    use crate::backend::PixmapHandle;
    let mut table = ResourceTable::new();
    let owner = ClientId(7);
    let other = ClientId(8);

    // owner: 2 GCs + 1 pixmap. other: 1 GC.
    table.gcs.insert(
        0x0700_0001,
        Gc::with_defaults(ResourceId(0x0700_0001), ROOT_WINDOW, owner),
    );
    table.gcs.insert(
        0x0700_0002,
        Gc::with_defaults(ResourceId(0x0700_0002), ROOT_WINDOW, owner),
    );
    table.gcs.insert(
        0x0800_0001,
        Gc::with_defaults(ResourceId(0x0800_0001), ROOT_WINDOW, other),
    );
    table.pixmaps.insert(
        0x0700_0010,
        Pixmap {
            id: ResourceId(0x0700_0010),
            drawable: ROOT_WINDOW,
            width: 1,
            height: 1,
            depth: 24,
            owner,
            host_xid: Some(PixmapHandle::from_raw_for_test(0xb01)),
            composite_name: false,
        },
    );

    let counts = table.resource_counts_by_owner(owner);
    assert!(counts.contains(&("GC", 2)), "GC count: {counts:?}");
    assert!(counts.contains(&("PIXMAP", 1)), "PIXMAP count: {counts:?}");
    // Zero-count types are omitted entirely.
    assert!(
        !counts.iter().any(|(n, _)| *n == "WINDOW" || *n == "FONT"),
        "zero types omitted: {counts:?}"
    );
    // Counts are per-owner: `other`'s GC is not attributed to `owner`.
    let other_counts = table.resource_counts_by_owner(other);
    assert!(
        other_counts.contains(&("GC", 1)),
        "other GC: {other_counts:?}"
    );
}

#[test]
fn disconnect_cleanup_drains_every_xid_in_use_map_for_owner() {
    use crate::backend::{
        ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle, PictureHandle, PixmapHandle,
    };

    let mut table = ResourceTable::new();
    let owner = ClientId(7);
    let other = ClientId(8);

    // Windows handled separately via destroy_window; this test
    // focuses on the non-window family that `remove_non_window_
    // resources_owned_by` is responsible for.

    // Pixmap
    let pix_id = ResourceId(0x0700_0001);
    table.pixmaps.insert(
        pix_id.0,
        Pixmap {
            id: pix_id,
            drawable: ROOT_WINDOW,
            width: 1,
            height: 1,
            depth: 24,
            owner,
            host_xid: Some(PixmapHandle::from_raw_for_test(0xa01)),
            composite_name: false,
        },
    );

    // GC
    let gc_id = ResourceId(0x0700_0002);
    table
        .gcs
        .insert(gc_id.0, Gc::with_defaults(gc_id, ROOT_WINDOW, owner));

    // Font
    let font_id = ResourceId(0x0700_0003);
    table.fonts.insert(
        font_id.0,
        Font {
            id: font_id,
            name: "fixed".to_string(),
            host_xid: FontHandle::from_raw_for_test(0xa03),
            metrics: FontMetrics::default(),
            owner,
        },
    );

    // Cursor
    let cur_id = ResourceId(0x0700_0004);
    table.cursors.insert(
        cur_id.0,
        Cursor {
            id: cur_id,
            owner,
            host_xid: Some(CursorHandle::from_raw_for_test(0xa04)),
            name_atom: None,
            anim: false,
        },
    );

    // Colormap
    let cmap_id = ResourceId(0x0700_0005);
    table.colormaps.insert(
        cmap_id.0,
        Colormap {
            id: cmap_id,
            visual: ROOT_VISUAL,
            host_colormap_xid: Some(ColormapHandle::from_raw_for_test(0xa05)),
            owner,
        },
    );

    // Picture
    let pic_id = ResourceId(0x0700_0006);
    table.pictures.insert(
        pic_id.0,
        PictureState {
            client: owner,
            host_picture_xid: Some(PictureHandle::from_raw_for_test(0xa06)),
            host_owned_pixmap: None,
            kind: PictureKind::Sourceless,
            drawable: None,
            window: None,
        },
    );

    // GlyphSet
    let gs_id = ResourceId(0x0700_0007);
    table.glyphsets.insert(
        gs_id.0,
        GlyphSetState {
            client: owner,
            host_glyphset_xid: GlyphSetHandle::from_raw_for_test(0xa07),
        },
    );

    // Sanity: everything is currently in use, and a resource
    // owned by a DIFFERENT client (not the disconnecting one)
    // should survive cleanup.
    let other_gc = ResourceId(0x0800_0001);
    table
        .gcs
        .insert(other_gc.0, Gc::with_defaults(other_gc, ROOT_WINDOW, other));

    for id in [
        pix_id, gc_id, font_id, cur_id, cmap_id, pic_id, gs_id, other_gc,
    ] {
        assert!(
            table.xid_in_use(id),
            "pre-cleanup: xid 0x{:x} should be in use",
            id.0,
        );
    }

    // Run the disconnect cleanup for `owner`.
    let _ = table.remove_non_window_resources_owned_by(owner);

    // Every map drained for `owner`; `other`'s GC survived.
    for id in [pix_id, gc_id, font_id, cur_id, cmap_id, pic_id, gs_id] {
        assert!(
            !table.xid_in_use(id),
            "post-cleanup: xid 0x{:x} still in use — \
                 a resource map is missing from remove_non_window_resources_owned_by",
            id.0,
        );
    }
    assert!(
        table.xid_in_use(other_gc),
        "other client's GC must survive cleanup of `owner`",
    );
}

#[test]
fn reference_glyphset_alias_frees_host_only_after_last_alias() {
    let mut table = ResourceTable::new();
    table.create_glyphset(
        ResourceId(0x200),
        GlyphSetState {
            client: ClientId(1),
            host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(0xabc),
        },
    );

    assert!(table.reference_glyphset(ClientId(1), ResourceId(0x201), ResourceId(0x200)));
    assert_eq!(
        table
            .glyphset(ResourceId(0x201))
            .map(|g| g.host_glyphset_xid.as_raw()),
        Some(0xabc)
    );

    assert!(table.free_glyphset(ResourceId(0x200)).is_none());
    assert_eq!(
        table
            .free_glyphset(ResourceId(0x201))
            .map(|g| g.host_glyphset_xid.as_raw()),
        Some(0xabc)
    );
}

#[test]
fn remove_client_frees_shared_glyphset_once() {
    let mut table = ResourceTable::new();
    table.create_glyphset(
        ResourceId(0x200),
        GlyphSetState {
            client: ClientId(1),
            host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(0xabc),
        },
    );
    assert!(table.reference_glyphset(ClientId(1), ResourceId(0x201), ResourceId(0x200)));

    let removed = table.remove_non_window_resources_owned_by(ClientId(1));

    assert_eq!(removed.freed_glyphsets, vec![0xabc]);
    assert!(table.glyphset(ResourceId(0x200)).is_none());
    assert!(table.glyphset(ResourceId(0x201)).is_none());
}

#[test]
fn set_pixmap_host_xid_unknown_id_returns_false() {
    let mut table = ResourceTable::new();
    let result = table.set_pixmap_host_xid(
        ResourceId(0xDEAD),
        crate::backend::PixmapHandle::from_raw_for_test(0x1234),
    );
    assert!(!result);
}

#[test]
fn set_pixmap_host_xid_sets_value_and_returns_true() {
    let mut table = ResourceTable::new();
    let request = CreatePixmapRequest {
        pixmap: ResourceId(0x0020_0002),
        drawable: ROOT_WINDOW,
        width: 128,
        height: 128,
        depth: 24,
    };
    table.create_pixmap(ClientId(1), request);
    let result = table.set_pixmap_host_xid(
        ResourceId(0x0020_0002),
        crate::backend::PixmapHandle::from_raw_for_test(0x5678),
    );
    assert!(result);
    assert_eq!(
        table
            .pixmap(ResourceId(0x0020_0002))
            .unwrap()
            .host_xid
            .map(|h| h.as_raw()),
        Some(0x5678)
    );
}

#[test]
fn visual_table_seeds_all_advertised_visuals() {
    let t = ResourceTable::new();
    let root = t.visual(ROOT_VISUAL).expect("root visual seeded");
    assert_eq!(root.depth, 24);
    assert_eq!(root.alpha_mask, 0);
    assert_eq!(root.host_visual_xid, None);
    let argb = t.visual(ARGB_VISUAL).expect("argb visual seeded");
    assert_eq!(argb.depth, 32);
    assert_eq!(argb.alpha_mask, 0xff00_0000);
    assert_eq!(argb.host_visual_xid, None);
    let glmark = t.visual(GLMARK_VISUAL).expect("glmark visual seeded");
    assert_eq!(glmark.depth, 24);
    assert_eq!(glmark.alpha_mask, 0);
    assert_eq!(glmark.host_visual_xid, None);
}

#[test]
fn colormap_for_visual_returns_matching_entry() {
    let t = ResourceTable::new();
    assert_eq!(
        t.colormap_for_visual(ROOT_VISUAL).map(|c| c.id),
        Some(ROOT_COLORMAP)
    );
    assert_eq!(
        t.colormap_for_visual(ARGB_VISUAL).map(|c| c.id),
        Some(ARGB_COLORMAP)
    );
}

#[test]
fn set_visual_host_xid_persists() {
    let mut t = ResourceTable::new();
    assert!(t.set_visual_host_xid(ARGB_VISUAL, 0x4711));
    assert_eq!(
        t.visual(ARGB_VISUAL)
            .and_then(|v| v.host_visual_xid)
            .map(|h| h.as_raw()),
        Some(0x4711)
    );
    assert!(!t.set_visual_host_xid(ResourceId(0xdead), 0x42));
}

#[test]
fn set_colormap_host_xid_persists() {
    let mut t = ResourceTable::new();
    assert!(t.set_colormap_host_xid(ARGB_COLORMAP, 0x9999));
    assert_eq!(
        t.colormap(ARGB_COLORMAP)
            .and_then(|c| c.host_colormap_xid)
            .map(|h| h.as_raw()),
        Some(0x9999)
    );
}

#[test]
fn is_known_visual_distinguishes_table_entries() {
    let t = ResourceTable::new();
    assert!(t.is_known_visual(ROOT_VISUAL));
    assert!(t.is_known_visual(ARGB_VISUAL));
    assert!(t.is_known_visual(GLMARK_VISUAL));
    // 0 is the wire encoding for CopyFromParent — not in the table; the
    // CreateWindow handler validates separately and never queries this.
    assert!(!t.is_known_visual(ResourceId(0)));
    assert!(!t.is_known_visual(ResourceId(0xdead_beef)));
}

#[test]
fn resource_owner_returns_owner_across_table_kinds() {
    let mut t = ResourceTable::new();
    assert_eq!(t.resource_owner(ROOT_WINDOW), Some(SERVER_OWNER));
    t.create_pixmap(
        ClientId(7),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0700_0010),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
            depth: 24,
        },
    );
    assert_eq!(t.resource_owner(ResourceId(0x0700_0010)), Some(ClientId(7)));
    assert_eq!(t.resource_owner(ResourceId(0xdead_beef)), None);
}

#[test]
fn resource_owner_distinguishes_separate_clients() {
    // Regression test for the "shared retain bucket" bug: with two
    // retained clients (32 and 33) both holding a pixmap, killing
    // one of them by resource ID must not affect the other.
    let mut t = ResourceTable::new();
    t.create_pixmap(
        ClientId(32),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0200_0001),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
            depth: 24,
        },
    );
    t.create_pixmap(
        ClientId(33),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0210_0001),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
            depth: 24,
        },
    );
    assert_eq!(
        t.resource_owner(ResourceId(0x0200_0001)),
        Some(ClientId(32))
    );
    assert_eq!(
        t.resource_owner(ResourceId(0x0210_0001)),
        Some(ClientId(33))
    );
    // Removing one client's resources must leave the other's
    // intact.
    let _ = t.remove_non_window_resources_owned_by(ClientId(32));
    assert!(t.resource_owner(ResourceId(0x0200_0001)).is_none());
    assert_eq!(
        t.resource_owner(ResourceId(0x0210_0001)),
        Some(ClientId(33))
    );
}

#[test]
fn cursor_anim_flag_default_false_settable_and_freed() {
    let mut table = ResourceTable::new();
    let id = ResourceId(0x500);
    table.create_cursor(ClientId(1), id);
    assert!(!table.cursor_is_anim(id), "fresh cursor must not be anim");
    table.set_cursor_anim(id);
    assert!(table.cursor_is_anim(id));
    // Unknown id is never anim.
    assert!(!table.cursor_is_anim(ResourceId(0x501)));
    // create_glyph_cursor also defaults to false.
    let gid = ResourceId(0x502);
    table.create_glyph_cursor(ClientId(1), gid);
    assert!(!table.cursor_is_anim(gid));
    // After free, the cursor is gone; cursor_is_anim must not return true.
    table.free_cursor(id);
    assert!(!table.cursor_is_anim(id), "freed cursor must not be anim");
}
