use super::*;

/// Diagnostic helper: write the `CursorRecord`'s source BGRA bytes
/// (as received from the X11 client, before any `load_image` /
/// dumb-buffer copy) to a PPM. Used in `do_dump_scanout` to bisect
/// whether cursor corruption enters at upload time (load_image) or
/// upstream (engine.get_image / wire format).
pub(in crate::kms::render::backend) fn dump_cursor_record_to_ppm(
    path: &str,
    rec: &crate::kms::render::cursor::CursorRecord,
) -> io::Result<()> {
    use std::io::Write;
    let w = usize::from(rec.width);
    let h = usize::from(rec.height);
    if w == 0 || h == 0 || rec.bgra_bytes.len() < w * h * 4 {
        return Err(io::Error::other(format!(
            "bad cursor record: {}x{} bytes={}",
            rec.width,
            rec.height,
            rec.bgra_bytes.len()
        )));
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(format!("P6\n{w} {h}\n255\n").as_bytes())?;
    let mut row_buf = vec![0u8; w * 3];
    for y in 0..h {
        for x in 0..w {
            let pi = (y * w + x) * 4;
            let b = rec.bgra_bytes[pi];
            let g = rec.bgra_bytes[pi + 1];
            let r = rec.bgra_bytes[pi + 2];
            row_buf[x * 3] = r;
            row_buf[x * 3 + 1] = g;
            row_buf[x * 3 + 2] = b;
        }
        file.write_all(&row_buf)?;
    }
    Ok(())
}

/// Look up the X11 RENDER `PICTFORMAT` ID a picture was created
/// with. Returns `0` for the synthetic / missing cases (picture
/// xid is 0 = "no picture," non-Drawable variant, or the xid
/// isn't recorded). Used by the diagnostic `render_composite`
/// trace to show marco's declared sampling intent alongside the
/// drawable-depth-derived sampling shape v2 currently uses.
pub(in crate::kms::render::backend) fn picture_pict_format(
    core: &crate::kms::core::KmsCore,
    host_pic: u32,
) -> u32 {
    if host_pic == 0 {
        return 0;
    }
    match core.pictures.get(&host_pic) {
        Some(crate::kms::core::PictureRecord::Drawable { pict_format, .. }) => *pict_format,
        _ => 0,
    }
}

/// Per-drawable storage dump triggered by `Ctrl-Alt-F12` via the
/// input thread, mirroring `Ctrl-Alt-Enter` for scanout. Walks a
/// fixed-known set of
/// "interesting" drawables — root, COW, every redirected backing —
/// and writes each storage's content to a `yserver-drawable-…`
/// file in cwd. Each dump cycle increments a global counter so
/// repeated invocations don't clobber.
///
/// Filename layout:
///
/// ```text
/// yserver-drawable-{run}-root-{w}x{h}.ppm
/// yserver-drawable-{run}-cow-{w}x{h}.ppm
/// yserver-drawable-{run}-backing-W0x{w_xid}-B0x{b_xid}-{w}x{h}.ppm
/// ```
///
/// PPM (P6, RGB) is chosen for universal viewer support; the α
/// channel is *intentionally dropped* — the depth-24 padding-byte
/// question is settled separately (4d.6 + the sample-view fix) and
/// what we want to see here is whether `B` contains the window's
/// painted content at all. If a deeper α audit becomes useful later,
/// switching to PAM (P7 with TUPLTYPE=RGB_ALPHA) is a one-liner.
///
/// Reuses `RenderEngine::get_image` for the per-drawable readback so
/// staging-buffer allocation, layout transitions, fence sync, and
/// the BGRA8 → wire-byte pack all flow through the existing,
/// production-tested path. Each dump is one queue submit + one
/// fence wait, so the total stop-the-world time is `O(n)` Vk waits
/// — at ~5 ms per drawable on bee this is fine for diagnostic use.
pub(in crate::kms::render::backend) fn do_dump_drawables(
    backend: &mut KmsBackend,
) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
    let run = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);

    // Snapshot targets BEFORE touching the engine — `engine.get_image`
    // takes `&mut store + &mut platform`, so we can't hold any
    // shared borrow on `store` while iterating. Each tuple carries
    // everything the per-drawable loop needs: a human-readable label
    // for the filename, the DrawableId for the read, the depth (drives
    // wire-byte unpack), and the extent (drives the read rect + the
    // PPM header).
    #[derive(Debug)]
    struct DumpTarget {
        label: String,
        id: crate::kms::render::store::DrawableId,
        depth: u8,
        width: u32,
        height: u32,
    }
    let mut targets: Vec<DumpTarget> = Vec::new();
    let mut window_manifest = String::new();
    {
        // Scoped read-borrow on the store + core. The borrow ends
        // at the `}` so the mutable borrows below are free to fire.
        if let Some(root_id) = backend.store.lookup(backend.core.window_id)
            && let Some(d) = backend.store.get(root_id)
        {
            targets.push(DumpTarget {
                label: format!("root-0x{:x}", backend.core.window_id),
                id: root_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        if let Some(cow_id) = backend.cow_id
            && let Some(d) = backend.store.get(cow_id)
        {
            targets.push(DumpTarget {
                label: format!(
                    "cow-0x{:x}",
                    yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0
                ),
                id: cow_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        // Sorted iteration so re-running the dump gives the same
        // filename ordering — keeps diff-tooling stable across runs.
        let mut pairs: Vec<(u32, u32)> = backend
            .core
            .host_window_to_backing
            .iter()
            .map(|(&w, b)| (w, b.as_raw()))
            .collect();
        pairs.sort_by_key(|(w, _)| *w);
        for (w_xid, b_xid) in pairs {
            let Some(b_id) = backend.store.lookup(b_xid) else {
                continue;
            };
            let Some(d) = backend.store.get(b_id) else {
                continue;
            };
            targets.push(DumpTarget {
                label: format!("backing-W0x{w_xid:x}-B0x{b_xid:x}"),
                id: b_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        let mut windows: Vec<(u32, WindowGeometry)> = backend
            .windows
            .iter()
            .map(|(&xid, geom)| (xid, *geom))
            .collect();
        windows.sort_by_key(|(xid, _)| *xid);
        for (host_xid, geom) in windows {
            let leaf_id = backend.store.lookup(host_xid);
            let redirected_target = leaf_id.and_then(|id| backend.store.redirected_target(id));
            let resolved = backend.resolve_window_paint_target(host_xid, leaf_id, geom.depth);
            let is_top_level = backend.core.top_level_order.contains(&host_xid);
            use std::fmt::Write as _;
            let _ = writeln!(
                window_manifest,
                "host=0x{host_xid:x} parent={} top_level={} mapped={} depth={} geom=({},{} {}x{}) \
leaf_id={leaf_id:?} redirected_target={redirected_target:?} resolved={resolved:?}",
                geom.parent
                    .map(|p| format!("0x{p:x}"))
                    .unwrap_or_else(|| "None".to_string()),
                is_top_level,
                geom.mapped,
                geom.depth,
                geom.x,
                geom.y,
                geom.width,
                geom.height,
            );
        }
        // Per-window leaf storage. This is what the scene composite
        // samples for unredirected windows, so a "storage right /
        // screen wrong" vs "storage already wrong" split (e16 menu
        // hover items, 2026-06-04) needs these dumped alongside the
        // manifest. Dedup against root/cow/backings pushed above.
        {
            let seen: std::collections::HashSet<crate::kms::render::store::DrawableId> =
                targets.iter().map(|t| t.id).collect();
            let mut win_xids: Vec<u32> = backend.windows.keys().copied().collect();
            win_xids.sort_unstable();
            for w_xid in win_xids {
                let Some(leaf_id) = backend.store.lookup(w_xid) else {
                    continue;
                };
                if seen.contains(&leaf_id) {
                    continue;
                }
                let Some(d) = backend.store.get(leaf_id) else {
                    continue;
                };
                if d.storage.extent.width == 0 || d.storage.extent.height == 0 {
                    continue;
                }
                targets.push(DumpTarget {
                    label: format!("win-0x{w_xid:x}"),
                    id: leaf_id,
                    depth: d.depth,
                    width: d.storage.extent.width,
                    height: d.storage.extent.height,
                });
            }
        }
        // Optional full-store sweep (YSERVER_DUMP_ALL_DRAWABLES=1):
        // every xid-registered drawable, which adds the pixmaps no
        // other walk reaches (client bg/tile pixmaps — e16's menu
        // item images live ONLY here). Off by default to keep the
        // normal dump lean.
        if std::env::var("YSERVER_DUMP_ALL_DRAWABLES").is_ok_and(|v| v == "1") {
            let seen: std::collections::HashSet<crate::kms::render::store::DrawableId> =
                targets.iter().map(|t| t.id).collect();
            let mut xid_pairs: Vec<(u32, crate::kms::render::store::DrawableId)> =
                backend.store.xid_entries().collect();
            xid_pairs.sort_unstable_by_key(|(xid, _)| *xid);
            for (xid, id) in xid_pairs {
                if seen.contains(&id) {
                    continue;
                }
                let Some(d) = backend.store.get(id) else {
                    continue;
                };
                if d.storage.extent.width == 0 || d.storage.extent.height == 0 {
                    continue;
                }
                targets.push(DumpTarget {
                    label: format!("xid-0x{xid:x}"),
                    id,
                    depth: d.depth,
                    width: d.storage.extent.width,
                    height: d.storage.extent.height,
                });
            }
        }
        // Dedup recent-Present source dumps against drawables
        // already in the target list so we don't double-dump if a
        // recent offscreen happens to coincide with a registered
        // backing.
        let already: std::collections::HashSet<crate::kms::render::store::DrawableId> =
            targets.iter().map(|t| t.id).collect();
        // Recent non-COW PresentPixmap sources. This captures
        // compositor-stage pixmaps too, which matter for Cinnamon:
        // the menu can be visible on screen while living only in a
        // fullscreen stage pixmap that never becomes a normal window
        // backing. Keep the source keyed by both src and dst so the
        // filename names which stage/window it was presented into.
        for (idx, &(src_xid, dst_xid)) in backend.recent_present_pixmaps.iter().enumerate() {
            let Some(src_id) = backend.store.lookup(src_xid) else {
                continue;
            };
            if already.contains(&src_id) {
                continue;
            }
            let Some(d) = backend.store.get(src_id) else {
                continue;
            };
            targets.push(DumpTarget {
                label: format!("present-src-{idx}-0x{src_xid:x}-to-0x{dst_xid:x}"),
                id: src_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
    }

    if targets.is_empty() {
        return Err(io::Error::other("no drawable dump targets available"));
    }
    log::info!(
        "render do_dump_drawables: run={run} target_count={}",
        targets.len(),
    );
    if !window_manifest.is_empty() {
        let path = format!("./yserver-drawable-{run}-windows.txt");
        if let Err(e) = std::fs::write(&path, &window_manifest) {
            log::warn!("render do_dump_drawables: write {path}: {e}");
        } else {
            log::info!("render do_dump_drawables: wrote {path}");
        }
    }

    let mut wrote = 0_u32;
    let mut last_err: Option<io::Error> = None;
    for t in targets {
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: t.width,
                height: t.height,
            },
        };
        backend
            .telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::ImageText);
        let bytes = match backend.engine.get_image(
            &mut backend.store,
            &mut backend.platform,
            Src::server_internal(t.id),
            rect,
            t.depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                let err = io::Error::other(format!("get_image {} ({:?}): {e:?}", t.label, t.id));
                log::warn!("render do_dump_drawables: {err}");
                last_err = Some(err);
                continue;
            }
        };
        let path = format!(
            "./yserver-drawable-{run}-{label}-{w}x{h}.ppm",
            label = t.label,
            w = t.width,
            h = t.height
        );
        if let Err(e) = write_drawable_ppm(&path, &bytes, t.width, t.height, t.depth) {
            log::warn!("render do_dump_drawables: write {path}: {e}");
            last_err = Some(e);
            continue;
        }
        log::info!(
            "render do_dump_drawables: wrote {path} (depth={} bytes={})",
            t.depth,
            bytes.len(),
        );
        wrote += 1;
    }
    if wrote > 0 {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| io::Error::other("no drawables dumped")))
    }
}

/// Write a single drawable's storage content as PAM (P7,
/// `RGB_ALPHA`) for depth-24 / depth-32 BGRA8 drawables (preserves
/// the α byte so a later analysis can see whether stored α is zero
/// / one / noise — the Stage 4d "shadow only" diagnosis needs to
/// distinguish "RGB looks right but α is zero" from "RGB itself is
/// broken"), or PGM (P5, gray) for depth-1 / depth-8 R8 drawables.
/// PAM is Netpbm's anymap format; ImageMagick / GIMP / most viewers
/// handle it transparently and dispatch on the magic number, not
/// the file extension.
///
/// `bytes` is the wire-packed buffer returned by
/// `RenderEngine::get_image`:
/// - depth 24/32: 4 bytes/pixel, X11 wire order (B, G, R, X|A) per
///   `pack_from_storage`'s BGRA8 → wire mapping.
/// - depth 8:     1 byte/pixel, R-channel.
/// - depth 1:     bit-packed MSB-first (rendered as PGM after
///   bit-expand, mostly for completeness — no real consumer of the
///   v2 dump runs depth-1 backings).
fn write_drawable_ppm(
    path: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
    depth: u8,
) -> io::Result<()> {
    use std::io::Write;

    let w = usize::try_from(width).map_err(|e| io::Error::other(format!("width: {e}")))?;
    let h = usize::try_from(height).map_err(|e| io::Error::other(format!("height: {e}")))?;
    let mut file = std::fs::File::create(path)?;
    match depth {
        24 | 32 => {
            // BGRA8 wire → PAM RGBA. Reorder per pixel: src is
            // (B, G, R, X|A) in storage byte order; PAM tuples
            // emit (R, G, B, A).
            let expected = w
                .checked_mul(h)
                .and_then(|p| p.checked_mul(4))
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(
                format!(
                    "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
                )
                .as_bytes(),
            )?;
            let mut row = vec![0u8; w * 4];
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 4;
                    let dst = x * 4;
                    row[dst] = bytes[src + 2]; // R
                    row[dst + 1] = bytes[src + 1]; // G
                    row[dst + 2] = bytes[src]; // B
                    row[dst + 3] = bytes[src + 3]; // A
                }
                file.write_all(&row)?;
            }
        }
        4 | 8 => {
            let expected = w
                .checked_mul(h)
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(format!("P5\n{width} {height}\n255\n").as_bytes())?;
            file.write_all(&bytes[..expected])?;
        }
        1 => {
            // Bit-packed MSB-first, padded to byte boundaries per
            // X11 wire spec for ZPixmap depth-1. Expand to PGM
            // bytes so a viewer can render the mask.
            let row_bytes = w.div_ceil(8);
            let expected = row_bytes
                .checked_mul(h)
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(format!("P5\n{width} {height}\n255\n").as_bytes())?;
            let mut out = vec![0u8; w];
            for y in 0..h {
                for x in 0..w {
                    let byte = bytes[y * row_bytes + (x / 8)];
                    let bit = byte >> (7 - (x % 8)) & 1;
                    out[x] = if bit == 1 { 255 } else { 0 };
                }
                file.write_all(&out)?;
            }
        }
        other => {
            return Err(io::Error::other(format!(
                "unsupported depth {other} for drawable dump",
            )));
        }
    }
    Ok(())
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_dump_dump_drawables(&mut self) {
        if let Err(e) = do_dump_drawables(self) {
            log::warn!("render dump_drawables: {e}");
        }
        // Stage 4d shadow-hunt: COW vs scanout vs present-src must
        // come from the same instant or the comparison is useless
        // (the moment of interest is the first COW-targeted
        // Present after caja paints, which moves on every frame).
        // Pair the scanout dump with the drawable dump so a single
        // Ctrl+Alt+F12 captures all three artifacts atomically.
        if let Err(e) = do_dump_scanout(self) {
            log::warn!("render dump_drawables: scanout side: {e}");
        }
        // Surface the COW + present-src ring state so the user can
        // tell at-a-glance whether the dump captured the expected
        // shape (cow_id set, recent sources non-empty) without
        // having to grep for the per-target log lines.
        log::info!(
            "render dump_drawables: cow_id={:?} recent_present_pixmaps_len={}",
            self.cow_id,
            self.recent_present_pixmaps.len(),
        );
    }
}
