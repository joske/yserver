use super::*;

// ── #137 tier 1: a uniform drawable glyph source ───────────────
//
// Java2D paints all text through `XRSolidSrcPict` — a 1x1 pixmap
// picture with `repeat=Normal` — rather than `CreateSolidFill`. The
// glyph path accepted only SolidFill and Gradient sources, so every
// Java/AWT text draw was silently discarded (#137).
//
// A source whose sampled domain is one pixel under a repeat that
// covers the plane is a constant colour, so collapsing it to a
// foreground colour is exact. This is the ORDERED-READBACK half of
// that proof: `get_image` is a Vulkan synchronisation and readback
// path, so a recording-backend test would assert nothing about the
// part most likely to be wrong. The byte -> premultiplied-`[f32; 4]`
// conversion is pinned separately and purely, in
// `engine::tests::premul_from_wire`.
//
// The reference colour throughout: premultiplied opaque
// R=0x33 G=0x88 B=0xCC. Deliberately non-grey and non-symmetric, so
// a channel swap cannot pass. Painted `Over` through a fully opaque
// glyph, the result is that colour exactly.
const GLYPH_SRC_R: u8 = 0x33;
const GLYPH_SRC_G: u8 = 0x88;
const GLYPH_SRC_B: u8 = 0xCC;
/// X11 pixel `0xAARRGGBB` for the reference colour.
const GLYPH_SRC_PIXEL: u32 = 0xFF33_88CC;
/// The dst background: a colour the reference is nowhere near, so
/// "still the background" is unambiguous evidence of a drop.
const GLYPH_DST_PIXEL: u32 = 0xFF00_00FF;

/// A glyphset holding one 4x4 fully opaque A8 glyph at id 1.
/// Same body shapes as `composite_glyphs_clip_intersects_picture`.
fn opaque_4x4_glyphset(b: &mut KmsBackend) -> u32 {
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("render_create_glyphset")
        .expect("Some(GlyphSetHandle)");
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(4)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    add_body.extend_from_slice(&[0xFFu8; 16]); // 4x4, all opaque
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("render_add_glyphs");
    gs.as_raw()
}

/// A `w`x`h` pixmap picture filled with `pixel`, at `repeat`
/// (X RENDER `CPRepeat` value: 0 None, 1 Normal, 2 Pad, 3 Reflect).
/// Returns `(backing pixmap xid, picture xid)` — the pixmap so a test
/// can repaint it under the live picture, as Java does.
fn repeating_pixmap_source(
    b: &mut KmsBackend,
    w: u16,
    h: u16,
    pixel: u32,
    repeat: u32,
) -> (u32, u32) {
    let pix = b.create_pixmap(None, 32, w, h).expect("create_pixmap");
    let pix_xid = pix.as_raw();
    b.fill_rectangle(None, pix_xid, pixel, 0, 0, w, h)
        .expect("fill_rectangle source");
    let pic = b
        .render_create_picture(
            None,
            AnyHandle::Pixmap(pix),
            yserver_protocol::x11::RENDER_FMT_ARGB32,
            0x0001, // CPRepeat
            &repeat.to_le_bytes(),
        )
        .expect("render_create_picture source")
        .expect("Some(PictureHandle)")
        .as_raw();
    (pix_xid, pic)
}

/// Stamp glyph id 1 at dst (0, 0) from `src_pic` onto a fresh 4x4
/// background-filled pixmap and read the result back.
fn paint_one_glyph(b: &mut KmsBackend, gs: u32, src_pic: u32, mask_fmt: u32) -> Vec<u8> {
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap dst");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, GLYPH_DST_PIXEL, 0, 0, 4, 4)
        .expect("fill_rectangle dst");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(PictureHandle)");

    // One element, one glyph, pen at (0, 0). Element header:
    // count(u8) + 3 pad + dx(i16) + dy(i16); then 1 id byte padded
    // to 4.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 0, 0, 0]);

    b.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        3,  // Over
        src_pic,
        dst_pic.as_raw(),
        mask_fmt,
        gs,
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    b.get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)")
}

/// Every pixel of a 4x4 readback equals `want` (wire BGRA).
fn assert_all_pixels(out: &[u8], want: [u8; 4], what: &str) {
    for y in 0..4usize {
        for x in 0..4usize {
            let off = (y * 4 + x) * 4;
            assert_eq!(
                &out[off..off + 4],
                &want,
                "{what}: pixel ({x},{y}) is {:?}, expected {want:?}",
                &out[off..off + 4],
            );
        }
    }
}

/// The fix: a 1x1 drawable source under `Normal` / `Pad` / `Reflect`
/// paints exactly what the equivalent `CreateSolidFill` paints.
///
/// Only an absent Vulkan ICD may skip. Past that point every stage —
/// seed allocation, painting, readback, the assertions — FAILS rather
/// than skips: CI runs the ignored tests on lavapipe, so skipping on
/// any error is how a live proof goes vacuous.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn uniform_drawable_glyph_source_paints_like_a_solid_fill() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = opaque_4x4_glyphset(&mut b);

    // The reference: the same premultiplied colour as a SolidFill.
    // 16-bit LE channels, r g b a, already premultiplied on the wire.
    let solid = b
        .render_create_solid_fill(
            None,
            [
                GLYPH_SRC_R,
                GLYPH_SRC_R,
                GLYPH_SRC_G,
                GLYPH_SRC_G,
                GLYPH_SRC_B,
                GLYPH_SRC_B,
                0xFF,
                0xFF,
            ],
        )
        .expect("render_create_solid_fill")
        .expect("Some(PictureHandle)")
        .as_raw();
    let reference = paint_one_glyph(&mut b, gs, solid, 0);
    // Guard the oracle itself: an opaque glyph over the background
    // must have replaced it, or "identical to the reference" would be
    // satisfied by two equally broken runs.
    assert_all_pixels(
        &reference,
        [GLYPH_SRC_B, GLYPH_SRC_G, GLYPH_SRC_R, 0xFF],
        "SolidFill reference",
    );

    for (repeat, name) in [(1u32, "Normal"), (2, "Pad"), (3, "Reflect")] {
        let (_, src) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, repeat);
        let out = paint_one_glyph(&mut b, gs, src, 0);
        assert_eq!(
            out, reference,
            "a 1x1 drawable source under {name} must paint what the SolidFill painted"
        );
    }
}

/// The negatives. Under tier 1 none of these may paint: a test
/// asserting otherwise would be asserting tier 2 (general drawable
/// sources), which is future work with its own spec.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_non_uniform_or_unrepeated_drawable_glyph_source_still_drops() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = opaque_4x4_glyphset(&mut b);
    let untouched = [
        (GLYPH_DST_PIXEL & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 8) & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 16) & 0xff) as u8,
        0xFF,
    ];

    // RepeatNone reads transparent outside its single pixel, so the
    // correct result paints only the glyph area overlapping that
    // pixel — not a uniform colour. Collapsing it would paint the
    // whole glyph.
    let (_, none_1x1) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 0);
    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, none_1x1, 0),
        untouched,
        "a 1x1 source under RepeatNone",
    );

    // A sampled domain larger than one pixel is not one colour.
    let (_, two_by_two) = repeating_pixmap_source(&mut b, 2, 2, GLYPH_SRC_PIXEL, 1);
    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, two_by_two, 0),
        untouched,
        "a 2x2 source under RepeatNormal",
    );

    // `mask_format != 0` selects Xorg's accumulate-into-an-A8-mask
    // branch, which `render_composite_glyphs` does not implement —
    // it always takes the per-glyph shortcut. Admitting a new source
    // class into that deviation would broaden wrong behaviour.
    let (_, normal_1x1) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, normal_1x1, yserver_protocol::x11::RENDER_FMT_A8),
        untouched,
        "a 1x1 source under RepeatNormal at a non-zero mask_format",
    );
}

/// #137 invariant 4 — the colour is read per composite, never carried
/// across a repaint of the source drawable.
///
/// This is the shape Java actually uses: it repaints the SAME 1x1
/// pixmap to change text colour and reuses the picture. A value cached
/// at `CreatePicture`, or keyed on anything that does not move when the
/// pixels do, renders every subsequent run of text in the PREVIOUS
/// colour — which converts a total, obvious failure into an
/// intermittent wrong-colour one that is harder to notice and harder
/// to report than the bug being fixed.
///
/// Written before any cache exists, so it passes trivially today. That
/// is the point: it must be in place and green before a cache can make
/// it fail.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_repainted_uniform_glyph_source_paints_the_new_colour() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = opaque_4x4_glyphset(&mut b);
    let (src_pix, src_pic) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);

    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, src_pic, 0),
        [GLYPH_SRC_B, GLYPH_SRC_G, GLYPH_SRC_R, 0xFF],
        "the first paint, at the source's original colour",
    );

    // Repaint the backing pixmap under the live picture. Opaque
    // premultiplied R=0xEE G=0x11 B=0x55 — no channel shared with the
    // first colour, so a stale read cannot look like a pass.
    b.fill_rectangle(None, src_pix, 0xFFEE_1155, 0, 0, 1, 1)
        .expect("fill_rectangle repaint of the source");
    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, src_pic, 0),
        [0x55, 0x11, 0xEE, 0xFF],
        "the second paint must use the colour the source was repainted to",
    );
}

// ── #137 step 5: the uniform-glyph-source colour cache ──────────
//
// The cache exists for ONE reason, and it is not the copy it avoids:
// the pixel copy-out measured at 1.5 ms/s, which is noise. It is that
// `get_image` must `close_open_frame(CloseReason::SyncWait)` before it
// can wait on its readback fence, and on a text-heavy workload that
// close was ~75% of ALL frame closes (`frame_builder_opens=237
// closes=238`, `close_reasons[sync_wait=179]`), dragging
// `ops/frame_avg` down to 1.6 — the batching
// `composite_glyphs_via_frame_builder` exists to provide, gone.
//
// So the frame close is what these assert against. The staleness
// regression above (`a_repainted_uniform_glyph_source_paints_the_new_colour`)
// is the other half: it was written before any cache existed precisely
// so the cache would be added against a test that fails when it is
// wrong, and it must stay green.

/// A fresh 4x4 background-filled destination and its picture, kept
/// across several draws. `paint_one_glyph` allocates a new one per call
/// and reads it back; these tests need the SAME destination across two
/// draws with no readback in between, because a readback is itself a
/// `SyncWait` frame close and would swamp the oracle.
fn glyph_dst_picture(b: &mut KmsBackend) -> (u32, u32) {
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap dst");
    let dst_xid = dst_pix.as_raw();
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(PictureHandle)")
        .as_raw();
    (dst_xid, dst_pic)
}

/// One paint the shape a real client draws it: fill the destination,
/// then stamp glyph id 1 over it from `src_pic`. No readback — the
/// caller reads the counters instead.
///
/// The fill matters to the oracle. It is a paint op, so it leaves a
/// frame OPEN; the glyph draw that follows either closes it (a
/// source readback) or batches into it (a cache hit). Without a
/// preceding paint there may be no open frame to close and "no close
/// happened" would be vacuously true.
fn fill_then_stamp_one_glyph(b: &mut KmsBackend, gs: u32, src_pic: u32, dst: (u32, u32)) {
    let (dst_xid, dst_pic) = dst;
    b.fill_rectangle(None, dst_xid, GLYPH_DST_PIXEL, 0, 0, 4, 4)
        .expect("fill_rectangle dst");
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    // 23 = CompositeGlyphs8, 3 = Over, mask_format 0 = tier 1.
    b.render_composite_glyphs(None, 23, 3, src_pic, dst_pic, 0, gs, 0, 0, &items, 0, 0)
        .expect("render_composite_glyphs");
}

/// **The whole justification, asserted rather than assumed.** Two glyph
/// draws from the same unmodified 1x1 source: the first reads the pixel
/// and closes the open frame with `CloseReason::SyncWait`; the second
/// hits the cache, does not read, and must therefore leave the frame
/// open.
///
/// The cold draw's `>= 1` is not decoration — it guards the oracle. If
/// no frame were open at read time the warm assertion would hold
/// vacuously, and this test would claim a win it never measured.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_cached_uniform_glyph_source_skips_the_sync_wait_frame_close() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = opaque_4x4_glyphset(&mut b);
    let (src_pix, src_pic) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    let dst = glyph_dst_picture(&mut b);

    let base = b.telemetry_close_reason_sync_wait_for_tests();
    fill_then_stamp_one_glyph(&mut b, gs, src_pic, dst);
    let cold = b.telemetry_close_reason_sync_wait_for_tests();
    assert!(
        cold > base,
        "oracle guard: the COLD draw must read the source pixel and close \
         the open frame with SyncWait — it went {base} -> {cold}, so \
         'the warm draw closed nothing' would assert nothing"
    );

    fill_then_stamp_one_glyph(&mut b, gs, src_pic, dst);
    let warm = b.telemetry_close_reason_sync_wait_for_tests();
    assert_eq!(
        warm, cold,
        "a cache hit must skip get_image and therefore the SyncWait \
         frame close: {cold} -> {warm}"
    );

    // And the miss returns the moment the client recolours the source,
    // which is the correctness price of the cache being version-keyed.
    // Same counter, so this cannot be satisfied by a cache that simply
    // never reads again.
    b.fill_rectangle(None, src_pix, 0xFFEE_1155, 0, 0, 1, 1)
        .expect("fill_rectangle repaint of the source");
    let after_repaint_base = b.telemetry_close_reason_sync_wait_for_tests();
    fill_then_stamp_one_glyph(&mut b, gs, src_pic, dst);
    let recoloured = b.telemetry_close_reason_sync_wait_for_tests();
    assert!(
        recoloured > after_repaint_base,
        "a repainted source must MISS and read again: \
         {after_repaint_base} -> {recoloured}"
    );
}

/// The hit-rate telemetry, which is what makes the cache's
/// effectiveness visible in production and a client that defeats it
/// detectable. A repeated same-colour draw increments would-hit; a
/// recolour between draws increments would-miss.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_uniform_glyph_source_cache_counters_report_hits_and_misses() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = opaque_4x4_glyphset(&mut b);
    let (src_pix, src_pic) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    let dst = glyph_dst_picture(&mut b);

    let counters = |b: &KmsBackend| {
        (
            b.telemetry().lifetime.glyph_src_cache_hit,
            b.telemetry().lifetime.glyph_src_cache_miss,
        )
    };

    // Cold: one miss, no hit.
    let (h0, m0) = counters(&b);
    fill_then_stamp_one_glyph(&mut b, gs, src_pic, dst);
    let (h1, m1) = counters(&b);
    assert_eq!((h1 - h0, m1 - m0), (0, 1), "the cold draw must miss");

    // Repeated at the same colour: one hit, no miss.
    fill_then_stamp_one_glyph(&mut b, gs, src_pic, dst);
    let (h2, m2) = counters(&b);
    assert_eq!((h2 - h1, m2 - m1), (1, 0), "a repeat draw must hit");

    // Recoloured between draws: one miss, no hit. This is Java's
    // pattern, and it is the case the counters exist to make visible.
    b.fill_rectangle(None, src_pix, 0xFFEE_1155, 0, 0, 1, 1)
        .expect("fill_rectangle repaint of the source");
    fill_then_stamp_one_glyph(&mut b, gs, src_pic, dst);
    let (h3, m3) = counters(&b);
    assert_eq!(
        (h3 - h2, m3 - m2),
        (0, 1),
        "a recolour between draws must miss"
    );
}

/// The two key collisions, end to end on the live path — the pure
/// versions are in `kms::backend::uniform_glyph_source_cache_tests`.
///
/// Two independently created 1x1 pixmaps, each written exactly once,
/// sit at the SAME `content_version`. A cache keyed on the version
/// alone hands the second one the first one's colour, and the only
/// visible symptom is the wrong ink.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn two_uniform_glyph_sources_at_the_same_content_version_keep_their_own_colours() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = opaque_4x4_glyphset(&mut b);

    // Same construction, same number of writes, so the same
    // content_version — and no channel shared between the colours, so a
    // cross-hit cannot look like a pass.
    let (_, first) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    let (_, second) = repeating_pixmap_source(&mut b, 1, 1, 0xFFEE_1155, 1);

    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, first, 0),
        [GLYPH_SRC_B, GLYPH_SRC_G, GLYPH_SRC_R, 0xFF],
        "the first source's own colour",
    );
    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, second, 0),
        [0x55, 0x11, 0xEE, 0xFF],
        "a DIFFERENT drawable at the same content_version must not \
         inherit the first source's cached colour",
    );
    // And back again, so this cannot pass by the cache simply never
    // hitting in one direction.
    assert_all_pixels(
        &paint_one_glyph(&mut b, gs, first, 0),
        [GLYPH_SRC_B, GLYPH_SRC_G, GLYPH_SRC_R, 0xFF],
        "the first source again, after the second was cached",
    );
}

// ── #137: an ARGB32 (subpixel-AA) glyph, end to end ─────────────
//
// This is where every part of the component-alpha path meets, at
// runtime, and the only place several of them are observable at all:
//
//   * an `ARGB32` glyph keeps its four channels through
//     `parse_add_glyphs`, and the engine packs them as FOUR
//     horizontally adjacent atlas coverage planes (logical R, G, B,
//     A) at packed width `4 * w`;
//   * the fragment shader fetches all four and emits the per-channel
//     alpha factor to output INDEX 1, which the pipeline's `SRC1_*`
//     dual-source blend factors consume;
//   * `AtlasEntry.packed_w` (the atlas footprint) and `logical_w`
//     (the glyph's own size) really differ here — this is the first
//     and only path where they do, so it is the only place confusing
//     them is reachable.
//
// The fixture mimics a real subpixel glyph as measured on OpenJDK
// 25: **alpha 255 on every pixel**, all the coverage in R, G and B.
// So a regression to reading the alpha byte paints every pixel of
// the glyph box at full coverage — a solid block, which is exactly
// the reported defect — and assertion 1 below fails.
//
// The channel values of a column are mutually distinct, which is
// what separates a real component-alpha composite from a convincing
// approximation of one: the grayscale reduction gives all three
// channels the SAME coverage (their mean), so its result and this
// one disagree on every column but the two flat ones. Assertion 3
// checks the exact per-channel values and assertion 4 checks that
// they are not the grayscale ones.

/// The ARGB32 fixture glyph: 8 wide so `w / 4` (2) and `4 * w`
/// (32) are both distinguishable from `w` in the painted extent,
/// and 4 high so a transposed read cannot fit.
const ARGB32_GLYPH_W: u16 = 8;
const ARGB32_GLYPH_H: u16 = 4;

/// One column of the fixture: wire `[B, G, R]` (alpha is 255 on
/// every pixel) and the A8 coverage the GRAYSCALE reduction would
/// produce from it.
///
/// Under component alpha the three wire bytes ARE the three
/// channels' coverages — blue's coverage is `wire[0]`, green's
/// `wire[1]`, red's `wire[2]` — and the trailing number is only used
/// as the counter-oracle: the value the `dualSrcBlend`-less fallback
/// would give all three channels instead. It is hand-computed from
/// `(r + g + b + 1) / 3` and written out, not read back from the
/// function under test — a table derived from the code it checks
/// asserts nothing.
///
/// The three channels of a column are mutually distinct, which is
/// what makes an R↔B swap, a first-plane-only sample and a
/// grayscale mean each land on a different answer. Column 0 is fully
/// zero and column 7 is saturated, so the ramp spans the whole
/// range.
const ARGB32_RAMP: [([u8; 3], u8); 8] = [
    //  B     G     R      cov    r+g+b -> (sum + 1) / 3
    ([0x00, 0x00, 0x00], 0x00), //   0 ->   0
    ([0x10, 0x20, 0x30], 0x20), //  96 ->  32
    ([0x20, 0x40, 0x60], 0x40), // 192 ->  64
    ([0x50, 0x78, 0xA0], 0x78), // 360 -> 120
    ([0x40, 0x80, 0xC0], 0x80), // 384 -> 128
    ([0x50, 0xA0, 0xF0], 0xA0), // 480 -> 160
    ([0x80, 0xC0, 0xFF], 0xC0), // 575 -> 192
    ([0xFE, 0xFF, 0xFF], 0xFF), // 764 -> 255
];

/// The one column whose blended result is exactly representable on
/// ALL THREE channels under component alpha, so it can be asserted
/// byte-for-byte rather than within a rounding slack.
///
/// Column 3 is `B = 0x50 (80)`, `G = 0x78 (120)`, `R = 0xA0 (160)`.
/// Each channel's `Over` numerator is a whole multiple of 255:
/// blue's `204*80 + 255*175`, green's `136*120` (its `dst` is 0) and
/// red's `51*160`. So the ideal result needs no rounding at all —
/// 239, 64, 32.
const ARGB32_EXACT_COL: usize = 3;

/// A glyphset in `ARGB32` holding two `8x4` glyphs, both with alpha
/// `0xFF` on every pixel and `x_off = 8`: id 1 is the coverage ramp
/// of `ARGB32_RAMP`, id 2 is saturated (`R = G = B = 0xFF`, so it
/// reduces to coverage `0xFF`).
///
/// Id 2 exists to be id 1's RIGHT-HAND NEIGHBOUR in the atlas —
/// see `intern_argb32_neighbour`.
///
/// Rows are dense at `w * 4` bytes with no row padding, in memory
/// order `[B, G, R, A]` — X RENDER `PICT_a8r8g8b8` is a
/// little-endian CARD32 with alpha at bits 24-31, so blue is the
/// low byte. Same body shape as `opaque_4x4_glyphset`.
fn argb32_ramp_glyphset(b: &mut KmsBackend) -> u32 {
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_ARGB32)
        .expect("render_create_glyphset")
        .expect("Some(GlyphSetHandle)");
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&2_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1, the ramp
    add_body.extend_from_slice(&2_u32.to_le_bytes()); // id = 2, saturated
    for _ in 0..2 {
        add_body.extend_from_slice(&u16::to_le_bytes(ARGB32_GLYPH_W)); // width
        add_body.extend_from_slice(&u16::to_le_bytes(ARGB32_GLYPH_H)); // height
        add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
        add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
        add_body.extend_from_slice(&i16::to_le_bytes(ARGB32_GLYPH_W as i16)); // x_off
        add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    }
    for _ in 0..ARGB32_GLYPH_H {
        for (wire, _) in ARGB32_RAMP {
            add_body.extend_from_slice(&[wire[0], wire[1], wire[2], 0xFF]);
        }
    }
    for _ in 0..ARGB32_GLYPH_H {
        for _ in 0..ARGB32_GLYPH_W {
            add_body.extend_from_slice(&[0xFF; 4]);
        }
    }
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("render_add_glyphs");
    gs.as_raw()
}

/// Intern glyph 1 and then glyph 2, in that order, so the saturated
/// glyph 2 lands immediately to the RIGHT of the ramp in the shared
/// R8 atlas: the shelf packer places same-height glyphs edge to
/// edge with no gutter.
///
/// Without that neighbour the "not `4 * w` wide" half of the extent
/// assertion has no teeth: an over-wide quad would sample
/// never-written atlas pixels, read coverage 0 there, and leave the
/// dst looking correct.
///
/// What the mutations actually showed, recorded so nobody re-derives
/// it: `logical_w = 4 * g.w` at the upload site alone does NOT reach
/// the dst either way, because `render_composite_glyphs` scissors to
/// a glyph union computed from the PROTOCOL width — that clip, not
/// the assertion, is what contains an entry-only width leak. The
/// extent assertion bites when the union widens too (union at
/// `4 * p.w` plus the over-wide quad paints this neighbour's
/// saturated coverage into column 8, and the test fails), and it
/// bites on its own for the opposite leak, `logical_w = g.w / 4`,
/// which paints only two columns.
fn intern_argb32_neighbour(b: &mut KmsBackend, gs: u32, src_pic: u32) {
    let scratch = b.create_pixmap(None, 32, 32, 4).expect("create_pixmap");
    let scratch_xid = scratch.as_raw();
    b.fill_rectangle(None, scratch_xid, GLYPH_DST_PIXEL, 0, 0, 32, 4)
        .expect("fill_rectangle scratch");
    let scratch_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(scratch), 0, 0, &[])
        .expect("render_create_picture scratch")
        .expect("Some(PictureHandle)");
    // One element, two glyphs: ids 1 then 2, the pen advancing by
    // `x_off`. Id bytes are padded to a 4-byte boundary.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[2u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 2, 0, 0]);
    b.render_composite_glyphs(
        None,
        23,
        3,
        src_pic,
        scratch_pic.as_raw(),
        0,
        gs,
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs warm-up");
}

/// Stamp glyph id 1 at dst (0, 0) from `src_pic` onto a fresh
/// `16x4` background-filled pixmap and read the result back. The
/// dst is twice the glyph's width so both "the glyph landed `4w`
/// wide" and "the glyph landed `w / 4` wide" are visible as painted
/// or unpainted columns rather than as clipping.
///
/// `mask_format = 0`, which is what Java sends.
fn paint_ramp_glyph(b: &mut KmsBackend, gs: u32, src_pic: u32) -> Vec<u8> {
    let dst_pix = b.create_pixmap(None, 32, 16, 4).expect("create_pixmap dst");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, GLYPH_DST_PIXEL, 0, 0, 16, 4)
        .expect("fill_rectangle dst");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(PictureHandle)");

    // One element, one glyph, pen at (0, 0) — same wire shape as
    // `paint_one_glyph`.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 0, 0, 0]);

    b.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        3,  // Over
        src_pic,
        dst_pic.as_raw(),
        0, // mask_format 0 — the per-glyph branch Java takes
        gs,
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    b.get_image_pixels_for_tests(dst_xid, 2, 0, 0, 16, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)")
}

/// `Over` with an opaque premultiplied source through an A8
/// coverage: `dst = src * cov + dst * (1 - cov)` per channel,
/// rounded to nearest.
fn over_at_coverage(src: u8, dst: u8, cov: u8) -> u8 {
    let cov = u32::from(cov);
    let n = u32::from(src) * cov + u32::from(dst) * (255 - cov);
    u8::try_from((n + 127) / 255).expect("a weighted mean of two bytes is a byte")
}

/// **The test the whole component-alpha step exists to pass.** An
/// `ARGB32` glyphset paints **per-channel coverage at its logical
/// width**, through the engine.
///
/// Under X RENDER component alpha, with an opaque premultiplied
/// source and `Over`, each channel blends with its OWN coverage:
///
/// ```text
/// dst.C = fg.C * cov.C + dst.C * (1 - fg.a * cov.C)
/// ```
///
/// and `cov.C` is the wire byte of that channel — blue's from
/// `wire[0]`, green's from `wire[1]`, red's from `wire[2]`, because
/// `PICT_a8r8g8b8` is a little-endian CARD32 with blue in the LOW
/// byte. The four things asserted, in order:
///
/// 1. the painted columns show VARYING coverage — the original
///    regression guard, because reading the alpha byte again would
///    paint a uniform block;
/// 2. the glyph lands `w` pixels wide — not `4 * w` (the packed
///    atlas footprint leaking into the geometry) and not `w / 4`;
///    the columns just past `w` must still be untouched background;
/// 3. **every column's exact per-channel bytes**, from the equation
///    above. Not "the channels differ": a red/blue swap satisfies
///    that, and it is the single most likely mistake in the pack. The
///    one column whose ideal result needs no rounding at all is
///    asserted byte-for-byte; the rest carry one LSB of slack for a
///    float blend rounded into UNORM8.
/// 4. and that the result is **not** what the grayscale reduction
///    would give. A `dualSrcBlend`-less device paints all three
///    channels with the mean of the three wire bytes; that is a
///    perfectly plausible-looking antialiased glyph, and only this
///    assertion separates it from a real subpixel composite.
///
/// The source is the asymmetric reference colour, so a channel
/// mistake in the SOURCE path cannot hide a coverage mistake — and
/// note the deliberate blue background: the glyph's blue coverage
/// column is the one channel the background also has, so an R/B
/// confusion anywhere moves a value.
///
/// Only an absent Vulkan ICD may skip; past that point every stage
/// FAILS rather than skips.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn an_argb32_glyph_paints_varying_coverage_at_its_logical_width() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = argb32_ramp_glyphset(&mut b);
    let (_, src) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    intern_argb32_neighbour(&mut b, gs, src);
    let out = paint_ramp_glyph(&mut b, gs, src);

    let background = [
        (GLYPH_DST_PIXEL & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 8) & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 16) & 0xff) as u8,
        0xFF,
    ];
    let px = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // ── (3) the exact per-channel values, and (4) that they are
    // not the grayscale ones.
    //
    // `wire` is `[B, G, R]`; the readback is BGRA. So channel 0 gets
    // the glyph's blue coverage, channel 1 its green and channel 2
    // its red — a swap in the pack moves column 1's blue from 252 to
    // 248 and its red from 10 to 3.
    //
    // The alpha channel stays 0xFF: this destination is opaque, so
    // `cov_a + dst.a * (1 - cov_a)` is 1 whatever the glyph's alpha
    // is. `cov_a` is pinned instead by
    // `a_component_alpha_glyph_samples_all_four_planes_at_the_plane_stride`,
    // over a TRANSPARENT destination where it survives the blend.
    for y in 0..ARGB32_GLYPH_H as usize {
        for (x, (wire, mean)) in ARGB32_RAMP.iter().enumerate() {
            let want = [
                over_at_coverage(GLYPH_SRC_B, background[0], wire[0]),
                over_at_coverage(GLYPH_SRC_G, background[1], wire[1]),
                over_at_coverage(GLYPH_SRC_R, background[2], wire[2]),
                0xFF,
            ];
            let got = px(x, y);
            if x == ARGB32_EXACT_COL {
                assert_eq!(
                    got, want,
                    "column {x} (per-channel coverage {wire:02x?}) must be exactly \
                     the component-alpha Over result",
                );
            } else {
                for (c, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                    assert!(
                        g.abs_diff(*w) <= 1,
                        "({x},{y}) channel {c} is {g}, expected {w} for per-channel \
                         coverage {wire:02x?} — whole pixel {got:?} vs {want:?}",
                    );
                }
            }

            // (4) the counter-oracle. The `dualSrcBlend`-less
            // fallback gives all three channels the MEAN of the
            // three wire bytes; where that differs from the
            // per-channel answer, the destination must not hold it.
            let grayscale = [
                over_at_coverage(GLYPH_SRC_B, background[0], *mean),
                over_at_coverage(GLYPH_SRC_G, background[1], *mean),
                over_at_coverage(GLYPH_SRC_R, background[2], *mean),
                0xFF,
            ];
            if grayscale
                .iter()
                .zip(want.iter())
                .any(|(g, w)| g.abs_diff(*w) > 1)
            {
                assert_ne!(
                    got, grayscale,
                    "({x},{y}) is the GRAYSCALE result {grayscale:?} — the three \
                     channels were given one shared coverage (the mean {mean:#04x}) \
                     instead of their own",
                );
            }
        }
    }

    // Teeth on (4): the two oracles really do disagree somewhere, so
    // the assertion above is not vacuous on this fixture.
    assert!(
        ARGB32_RAMP.iter().any(|(wire, mean)| {
            over_at_coverage(GLYPH_SRC_R, background[2], wire[2]).abs_diff(over_at_coverage(
                GLYPH_SRC_R,
                background[2],
                *mean,
            )) > 1
        }),
        "fixture: per-channel and grayscale coverage must differ on some column",
    );

    // ── (1) varying, not a block. The alpha byte is 255 on every
    // pixel of this glyph, so reading it paints all 8 columns the
    // full source colour: one distinct value, and equal to the
    // source.
    let mut distinct: Vec<[u8; 4]> = Vec::new();
    for x in 0..ARGB32_GLYPH_W as usize {
        let v = px(x, 0);
        if !distinct.contains(&v) {
            distinct.push(v);
        }
    }
    assert!(
        distinct.len() >= 6,
        "the glyph painted only {} distinct values across {ARGB32_GLYPH_W} \
         ramped columns ({distinct:?}) — a solid block means the glyph's \
         ALPHA byte is being read again instead of its R, G, B coverage",
        distinct.len(),
    );
    let full_source = [GLYPH_SRC_B, GLYPH_SRC_G, GLYPH_SRC_R, 0xFF];
    for (x, (wire, _)) in ARGB32_RAMP
        .iter()
        .enumerate()
        .take(ARGB32_RAMP.len() - 1)
        .skip(1)
    {
        assert_ne!(
            px(x, 0),
            full_source,
            "column {x} painted at FULL coverage; its ramped per-channel \
             coverage is {wire:02x?}",
        );
    }

    // ── (2) the extent is `w`, not `4 * w` and not `w / 4`.
    //
    // The last column is saturated coverage, so the glyph's right
    // edge is the exact source colour: a `w / 4`-wide glyph would
    // leave it at the background.
    assert_eq!(
        px(ARGB32_GLYPH_W as usize - 1, 0),
        full_source,
        "the glyph's last column must be painted at full coverage — a glyph \
         laid down `w / 4` wide never reaches it",
    );
    // And nothing past `w` was touched: a glyph laid down `4 * w`
    // wide, or one whose atlas UV span came from `packed_w`, paints
    // into these columns.
    for y in 0..ARGB32_GLYPH_H as usize {
        for x in ARGB32_GLYPH_W as usize..16 {
            assert_eq!(
                px(x, y),
                background,
                "({x},{y}) is past the glyph's logical width and must be \
                 untouched background",
            );
        }
    }
}

// ── #137: the component-alpha COORDINATE path, in isolation ─────
//
// The sibling ramp test above proves the four planes are packed and
// blended correctly, but it varies coverage along x, so a fetch that
// lands one texel out is only ever one ramp step wrong. Here the four
// planes hold four DISTINCT CONSTANTS instead, so each way of getting
// the addressing wrong lands on its own recognisable answer:
//
//   * plane order R↔B swapped     → blue 120, red 5   (not 20, 30)
//   * plane stride zeroed         → every plane reads R: 120, 80, 30, 150
//   * local offset half a texel out → the rightmost column reads the
//     NEXT plane (red 9) and the alpha plane reads the neighbour glyph
//   * `4 * w` as the quad width   → columns 4..16 get painted
//
// The destination is **transparent**, which is what makes the fourth
// plane observable at all: under `Over` onto an opaque destination
// `cov_a + dst.a * (1 - cov_a)` is 1 whatever the glyph's alpha is,
// so `cov_a` could be a hardcoded 1 and nothing would move. Onto a
// transparent one the result alpha IS `cov_a`.

/// Wire `[B, G, R, A]` of the constant-plane fixture glyph, and the
/// exact `Over`-onto-transparent result each produces from the
/// reference source colour.
///
/// Every coverage is chosen so `src.C * cov` is a whole multiple of
/// 255 — blue needs `cov % 5 == 0`, green `cov % 15 == 0`, red
/// `cov % 5 == 0` against `0xCC`, `0x88`, `0x33` — so the ideal
/// result needs no rounding and can be asserted byte-for-byte. And
/// the four coverages AND the four results are mutually distinct, so
/// no permutation of the planes produces the right answer.
const PLANE_CONST_WIRE: [u8; 4] = [25, 45, 150, 200];
/// `[B, G, R, A]` of the painted pixel: `0xCC*25/255 = 20`,
/// `0x88*45/255 = 24`, `0x33*150/255 = 30`, and alpha `= cov_a`.
const PLANE_CONST_RESULT: [u8; 4] = [20, 24, 30, 200];
const PLANE_CONST_W: u16 = 4;
const PLANE_CONST_H: u16 = 2;

/// An `ARGB32` glyphset holding three glyphs, all carrying
/// `PLANE_CONST_WIRE` on every pixel:
///
/// * id 1 — the `4x2` constant-plane fixture;
/// * id 2 — `4x2` saturated on every channel. It exists to be id 1's
///   right-hand atlas neighbour, so a read past id 1's packed
///   footprint lands on a known `0xFF` instead of on never-written
///   atlas memory that would read as coverage 0 and look correct;
/// * id 3 — **`1x1`**, i.e. plane stride 1, the tightest possible
///   packing. Its four planes are four consecutive texels, so any
///   off-by-one in the local offset immediately reads the next
///   plane, and it is also the one-pixel-wide case nothing else in
///   the suite exercises.
fn plane_constant_glyphset(b: &mut KmsBackend) -> u32 {
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_ARGB32)
        .expect("render_create_glyphset")
        .expect("Some(GlyphSetHandle)");
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&3_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id 1 — constants
    add_body.extend_from_slice(&2_u32.to_le_bytes()); // id 2 — saturated
    add_body.extend_from_slice(&3_u32.to_le_bytes()); // id 3 — 1x1
    for (w, h) in [
        (PLANE_CONST_W, PLANE_CONST_H),
        (PLANE_CONST_W, PLANE_CONST_H),
        (1, 1),
    ] {
        add_body.extend_from_slice(&u16::to_le_bytes(w));
        add_body.extend_from_slice(&u16::to_le_bytes(h));
        add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
        add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
        add_body.extend_from_slice(&i16::to_le_bytes(w as i16));
        add_body.extend_from_slice(&i16::to_le_bytes(0));
    }
    // Glyph bodies, in id order. Rows are dense at `w * 4` bytes.
    for _ in 0..PLANE_CONST_H {
        for _ in 0..PLANE_CONST_W {
            add_body.extend_from_slice(&PLANE_CONST_WIRE);
        }
    }
    for _ in 0..PLANE_CONST_H {
        for _ in 0..PLANE_CONST_W {
            add_body.extend_from_slice(&[0xFF; 4]);
        }
    }
    add_body.extend_from_slice(&PLANE_CONST_WIRE);
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("render_add_glyphs");
    gs.as_raw()
}

/// Stamp `glyph_ids` at successive pen positions onto a fresh
/// `16x4` pixmap pre-filled with `bg_pixel`, and read the result
/// back. `mask_format = 0` — what Java sends.
fn paint_glyphs_onto(
    b: &mut KmsBackend,
    gs: u32,
    src_pic: u32,
    bg_pixel: u32,
    ids: &[u8],
) -> Vec<u8> {
    let dst_pix = b.create_pixmap(None, 32, 16, 4).expect("create_pixmap dst");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, bg_pixel, 0, 0, 16, 4)
        .expect("fill_rectangle dst");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(PictureHandle)");

    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[u8::try_from(ids.len()).expect("few glyphs"), 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    let mut payload = ids.to_vec();
    while !payload.len().is_multiple_of(4) {
        payload.push(0);
    }
    items.extend_from_slice(&payload);

    b.render_composite_glyphs(
        None,
        23,
        3,
        src_pic,
        dst_pic.as_raw(),
        0,
        gs,
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    b.get_image_pixels_for_tests(dst_xid, 2, 0, 0, 16, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)")
}

/// The coordinate path alone: four planes of four distinct constants,
/// composited onto a **transparent** destination so all four —
/// including the glyph's own alpha — survive to the readback.
///
/// Asserts the exact `[B, G, R, A]` of every pixel of the glyph box,
/// and that nothing outside it moved. See the block comment above for
/// what each addressing mistake produces instead.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_component_alpha_glyph_samples_all_four_planes_at_the_plane_stride() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = plane_constant_glyphset(&mut b);
    let (_, src) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    // Intern id 1 then id 2, so the saturated glyph lands immediately
    // right of the fixture's 4-plane footprint in the shared atlas.
    let _ = paint_glyphs_onto(&mut b, gs, src, GLYPH_DST_PIXEL, &[1, 2]);
    // The real subject: id 1 alone, onto a fully transparent dst.
    let out = paint_glyphs_onto(&mut b, gs, src, 0x0000_0000, &[1]);

    let px = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Sanity on the fixture's own design: nothing is accidentally
    // equal, so no plane permutation and no shared-coverage answer
    // can coincide with the right one.
    let distinct = |mut v: Vec<u8>| {
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    assert_eq!(
        distinct(PLANE_CONST_WIRE.to_vec()),
        4,
        "fixture: the four coverages must be distinct",
    );
    assert_eq!(
        distinct(PLANE_CONST_RESULT.to_vec()),
        4,
        "fixture: the four results must be distinct",
    );

    for y in 0..PLANE_CONST_H as usize {
        for x in 0..PLANE_CONST_W as usize {
            assert_eq!(
                px(x, y),
                PLANE_CONST_RESULT,
                "({x},{y}): each channel must carry its OWN plane's coverage. \
                 Blue 120 / red 5 means the R and B planes are swapped; \
                 [120, 80, 30, 150] means the plane stride is zero and every \
                 fetch read the R plane; red 9 in the last column means the \
                 local offset is half a texel out; alpha 255 means `cov_a` was \
                 taken as a constant 1 instead of read from the fourth plane",
            );
        }
    }

    // Nothing outside the glyph's LOGICAL box moved. A quad laid down
    // at the packed width (4 * 4 = 16) covers this whole readback.
    for y in 0..4usize {
        for x in 0..16usize {
            if y < PLANE_CONST_H as usize && x < PLANE_CONST_W as usize {
                continue;
            }
            assert_eq!(
                px(x, y),
                [0, 0, 0, 0],
                "({x},{y}) is outside the glyph's logical {PLANE_CONST_W}x\
                 {PLANE_CONST_H} box and must be untouched — a quad at the \
                 packed width 4*w would paint it",
            );
        }
    }

    // ── the ONE-TEXEL-WIDE glyph, plane stride 1 ──
    //
    // Its four planes are four consecutive texels, which is the
    // tightest packing there is: the R plane's only texel is at the
    // atlas origin and the A plane's is three texels along. Nothing
    // else in the suite draws a one-pixel-wide glyph at all, so a
    // per-glyph path that silently skipped `logical_w == 1` — or a
    // stride that collapsed at width 1 — would go unnoticed.
    let tiny = paint_glyphs_onto(&mut b, gs, src, 0x0000_0000, &[3]);
    let tiny_px = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [tiny[off], tiny[off + 1], tiny[off + 2], tiny[off + 3]]
    };
    assert_eq!(
        tiny_px(0, 0),
        PLANE_CONST_RESULT,
        "a 1x1 component-alpha glyph must sample its four planes at stride 1 \
         — and must be drawn at all",
    );
    for y in 0..4usize {
        for x in 0..16usize {
            if (x, y) == (0, 0) {
                continue;
            }
            assert_eq!(
                tiny_px(x, y),
                [0, 0, 0, 0],
                "({x},{y}) is outside the 1x1 glyph and must be untouched — a \
                 quad at its packed width 4 would paint columns 1..4",
            );
        }
    }
}

// ── #137 invariant 1: the A8 path does not move ─────────────────
//
// The overwhelmingly common path. Component alpha added two vertex
// outputs, a fourth vertex attribute, a wider instance stride and a
// second fragment output, all of which the A8 specialization also
// carries — so "A8 is untouched" stopped being true by construction
// and became something to assert.
//
// The existing A8 glyph tests all use FULLY OPAQUE glyphs, where any
// coverage error above zero paints the same pixel; a ramp is what
// makes a wrong coverage visible. The oracle is analytic — the `Over`
// equation at a single shared coverage — rather than a captured
// golden, so it also fails if the pre-change output was itself wrong.

/// An `A8` glyphset holding one `8x4` glyph whose columns carry
/// exactly the coverages `ARGB32_RAMP`'s grayscale column names — so
/// this fixture is, pixel for pixel, what the `dualSrcBlend`-less
/// fallback turns the component-alpha fixture into.
fn a8_ramp_glyphset(b: &mut KmsBackend) -> u32 {
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("render_create_glyphset")
        .expect("Some(GlyphSetHandle)");
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(ARGB32_GLYPH_W));
    add_body.extend_from_slice(&u16::to_le_bytes(ARGB32_GLYPH_H));
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(ARGB32_GLYPH_W as i16));
    add_body.extend_from_slice(&i16::to_le_bytes(0));
    // Dense A8 rows; X RENDER pads a glyph row to a 4-byte boundary
    // and 8 already is one.
    for _ in 0..ARGB32_GLYPH_H {
        for (_, cov) in ARGB32_RAMP {
            add_body.push(cov);
        }
    }
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("render_add_glyphs");
    gs.as_raw()
}

/// An `A8` glyphset paints exactly what it always did: one shared
/// coverage per pixel across all three channels, at the glyph's own
/// width (design invariant 1).
///
/// Measured byte-identical on lavapipe against the pre-component-alpha
/// parent (`0fb89fdf`) — the same 8 painted columns, BGRA:
/// `[255,0,0] [249,17,6] [242,34,13] [231,64,24] [229,68,26]
/// [223,85,32] [217,102,38] [204,136,51]`, alpha `0xFF` throughout,
/// columns 8..16 untouched background. The assertions below are the
/// analytic form of that, so they also fail if the pre-change output
/// had itself been wrong.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn an_a8_glyphset_paints_byte_identical_single_plane_coverage() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs = a8_ramp_glyphset(&mut b);
    let (_, src) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);
    let out = paint_glyphs_onto(&mut b, gs, src, GLYPH_DST_PIXEL, &[1]);

    let background = [
        (GLYPH_DST_PIXEL & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 8) & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 16) & 0xff) as u8,
        0xFF,
    ];
    let px = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    for y in 0..ARGB32_GLYPH_H as usize {
        for (x, (_, cov)) in ARGB32_RAMP.iter().enumerate() {
            // ONE coverage for all three channels — the A8 semantic.
            // Component alpha's per-channel answer would differ on
            // every column but the flat ones.
            let want = [
                over_at_coverage(GLYPH_SRC_B, background[0], *cov),
                over_at_coverage(GLYPH_SRC_G, background[1], *cov),
                over_at_coverage(GLYPH_SRC_R, background[2], *cov),
                0xFF,
            ];
            let got = px(x, y);
            for (c, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                assert!(
                    g.abs_diff(*w) <= 1,
                    "({x},{y}) channel {c} is {g}, expected {w} at A8 coverage \
                     {cov:#04x} — whole pixel {got:?} vs {want:?}",
                );
            }
        }
        for x in ARGB32_GLYPH_W as usize..16 {
            assert_eq!(
                px(x, y),
                background,
                "({x},{y}) is past the A8 glyph's width and must be untouched",
            );
        }
    }
}

/// One `CompositeGlyphs` request that mixes formats, end to end.
///
/// Reachable in production for the first time as of component alpha:
/// before it, the upload reduced every ARGB32 glyph to one A8 plane, so
/// every glyph of a mixed request had the same effective layout and the
/// run splitter formed exactly one run. Now an A8 glyph and an ARGB32
/// glyph in one request need two pipelines, so the request records two
/// ops over one shared instance buffer — and each run must draw ITS
/// OWN instance range with ITS OWN pipeline.
///
/// The stream switches glyphset mid-stream through the inline
/// `count == 255` element, which is the only way a client can do this.
/// Both glyphs are asserted against their own exact oracle: the A8 one
/// gets one shared coverage on all three channels, the ARGB32 one gets
/// its four planes. So a second run drawn with the first run's pipeline
/// (or from the first run's instance range) lands on the wrong answer
/// for one of them.
///
/// It also asserts the request-wide half of the contract: however many
/// runs it split into, the client issued ONE request and gets ONE
/// returned region back.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn one_request_mixing_a8_and_component_alpha_glyphs_paints_both() {
    use yserver_core::backend::Backend;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: missing capability — no live Vulkan ICD: {e}");
            return;
        }
    };
    let gs_a8 = a8_ramp_glyphset(&mut b);
    let gs_ca = plane_constant_glyphset(&mut b);
    let (_, src) = repeating_pixmap_source(&mut b, 1, 1, GLYPH_SRC_PIXEL, 1);

    let dst_pix = b.create_pixmap(None, 32, 16, 4).expect("create_pixmap dst");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, GLYPH_DST_PIXEL, 0, 0, 16, 4)
        .expect("fill_rectangle dst");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture dst")
        .expect("Some(PictureHandle)");

    // Element 1: the A8 ramp glyph (id 1 of gs_a8) at pen 0. It is 8
    // wide with x_off 8, so the pen lands at 8.
    // Element 2: inline glyphset change to the ARGB32 set.
    // Element 3: its constant-plane glyph (id 1 of gs_ca) at pen 8.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    items.push(255);
    items.extend_from_slice(&[0u8, 0, 0]);
    items.extend_from_slice(&gs_ca.to_le_bytes());
    items.extend_from_slice(&[1u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 0, 0, 0]);

    let region = b
        .render_composite_glyphs(
            None,
            23, // CompositeGlyphs8
            3,  // Over
            src,
            dst_pic.as_raw(),
            0, // mask_format 0 — the per-glyph branch Java takes
            gs_a8,
            0,
            0,
            &items,
            0,
            0,
        )
        .expect("render_composite_glyphs");

    // ONE request, ONE region — whatever the run count. A per-run
    // region looks harmless and breaks a compositor's damage tracking.
    assert_eq!(
        region.len(),
        1,
        "a split request must still return exactly one region: {region:?}",
    );

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 16, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)");
    let background = [
        (GLYPH_DST_PIXEL & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 8) & 0xff) as u8,
        ((GLYPH_DST_PIXEL >> 16) & 0xff) as u8,
        0xFF,
    ];
    let px = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Run 1 — the A8 glyph, columns 0..8: ONE coverage for all three
    // channels. The component-alpha pipeline would give each channel
    // the wire byte of a plane that does not exist here.
    for y in 0..ARGB32_GLYPH_H as usize {
        for (x, (_, cov)) in ARGB32_RAMP.iter().enumerate() {
            let want = [
                over_at_coverage(GLYPH_SRC_B, background[0], *cov),
                over_at_coverage(GLYPH_SRC_G, background[1], *cov),
                over_at_coverage(GLYPH_SRC_R, background[2], *cov),
                0xFF,
            ];
            let got = px(x, y);
            for (c, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                assert!(
                    g.abs_diff(*w) <= 1,
                    "run 1 ({x},{y}) channel {c} is {g}, expected {w} at A8 \
                     coverage {cov:#04x} — whole pixel {got:?} vs {want:?}",
                );
            }
        }
    }

    // Run 2 — the component-alpha glyph at pen 8, columns 8..12: each
    // channel from its own plane. A run drawn from run 1's instance
    // range would paint the A8 glyph here instead, and a run drawn
    // with run 1's pipeline would read only the R plane.
    for y in 0..PLANE_CONST_H as usize {
        for x in 0..PLANE_CONST_W as usize {
            let want = [
                over_at_coverage(GLYPH_SRC_B, background[0], PLANE_CONST_WIRE[0]),
                over_at_coverage(GLYPH_SRC_G, background[1], PLANE_CONST_WIRE[1]),
                over_at_coverage(GLYPH_SRC_R, background[2], PLANE_CONST_WIRE[2]),
                0xFF,
            ];
            let got = px(8 + x, y);
            for (c, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                assert!(
                    g.abs_diff(*w) <= 1,
                    "run 2 ({},{y}) channel {c} is {g}, expected {w} — whole \
                     pixel {got:?} vs {want:?}",
                    8 + x,
                );
            }
        }
    }

    // Neither run painted outside its own box.
    for y in 0..4usize {
        for x in 0..16usize {
            let in_a8 = y < ARGB32_GLYPH_H as usize && x < ARGB32_GLYPH_W as usize;
            let in_ca = y < PLANE_CONST_H as usize && (8..8 + PLANE_CONST_W as usize).contains(&x);
            if in_a8 || in_ca {
                continue;
            }
            assert_eq!(
                px(x, y),
                background,
                "({x},{y}) is outside both glyph boxes and must be untouched",
            );
        }
    }
}

/// Stage 3d v1-bug-fix gate (plan §3d): v1's
/// `try_vk_render_composite_glyphs` reads but **ignores** the dst
/// picture's clip (`kms::backend.rs:5313`); v2 must honour it via
/// per-rect scissoring. The test stamps two 4×4 white glyphs at
/// dst (0, 0) and (4, 0) onto an 8×4 blue pixmap with the picture
/// clip set to the top-left 4×4 rect. Result: left half painted
/// white; right half stays blue. v1 would paint both glyphs.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_clip_intersects_picture() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 8×4 dst pixmap pre-filled with blue (pixel 0xFF0000FF).
    let dst_pix = b.create_pixmap(None, 32, 8, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 4)
        .expect("fill_rectangle pre");

    // SolidFill source: opaque premultiplied white (R=G=B=A=0xFFFF).
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");

    // Dst picture wrapping the pixmap.
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");

    // Picture clip: top-left 4×4 only.
    // Wire body for render_set_picture_clip_rectangles: picture(4)
    // + clip_x_origin(INT16) + clip_y_origin(INT16) + N×rectangles
    // (INT16 x, INT16 y, CARD16 w, CARD16 h).
    let mut clip_body: Vec<u8> = Vec::new();
    clip_body.extend_from_slice(&dst_pic.as_raw().to_le_bytes());
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // clip_x_origin
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // clip_y_origin
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // rect.x
    clip_body.extend_from_slice(&i16::to_le_bytes(0)); // rect.y
    clip_body.extend_from_slice(&u16::to_le_bytes(4)); // rect.w
    clip_body.extend_from_slice(&u16::to_le_bytes(4)); // rect.h
    b.render_set_picture_clip_rectangles(None, dst_pic.as_raw(), &clip_body)
        .expect("set_picture_clip_rectangles");

    // Glyphset with one 4×4 A8 glyph at id=1 (all 0xFF alpha,
    // x_off=4 so consecutive glyphs sit edge-to-edge).
    // RENDER_FMT_A8 = the standard a8 picture format id (depends
    // on the server's PictFormat catalogue; the backend's
    // render_create_glyphset matches on ynest_format constants).
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");

    // render_add_glyphs body shape (from parse_add_glyphs):
    // body_tail = n(u32) + n×id(u32) + n×info(12 bytes) +
    // n×pixels(stride×h).
    // info layout (per parse_add_glyphs): width(u16) height(u16)
    // x(i16) y(i16) x_off(i16) y_off(i16) — 12 bytes.
    // A8 stride for w=4: (4+3) & !3 = 4. Total pixel bytes = 4×4 = 16.
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // n
    add_body.extend_from_slice(&1_u32.to_le_bytes()); // id = 1
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // width
    add_body.extend_from_slice(&u16::to_le_bytes(4)); // height
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // x bearing
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y bearing
    add_body.extend_from_slice(&i16::to_le_bytes(4)); // x_off
    add_body.extend_from_slice(&i16::to_le_bytes(0)); // y_off
    add_body.extend_from_slice(&[0xFFu8; 16]); // pixels: 4×4 all opaque
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");

    // CompositeGlyphs8 items: one element with count=2 glyphs id=1
    // (pen starts at dx=0,dy=0, glyph 1 stamps at (0,0), pen
    // advances to (4,0), glyph 2 stamps at (4,0)).
    // Element header: count(u8) + 3 pad + dx(i16) + dy(i16) = 8 bytes.
    // Then 2 × 1-byte ids = 2 bytes, padded to 4. Total 12 bytes.
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[2u8, 0, 0, 0]); // count + pad
    items.extend_from_slice(&i16::to_le_bytes(0)); // dx
    items.extend_from_slice(&i16::to_le_bytes(0)); // dy
    items.extend_from_slice(&[1u8, 1, 0, 0]); // 2 ids + pad

    b.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        3,  // Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0, // mask_fmt — unused
        gs.as_raw(),
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 4, !0)
        .expect("get_image")
        .expect("Some(bytes)");

    // Left half (x=0..4): glyph painted white over blue with
    // premul srcover (atlas alpha 0xFF, foreground white) →
    // result white. Right half (x=4..8): clip excluded the glyph
    // → blue preserved. If v1's _clip-unused bug were present,
    // both halves would be white.
    for y in 0..4 {
        for x in 0..4u32 {
            let off = (y * 8 + x as usize) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0xFF, 0xFF, 0xFF],
                "left half should be white at ({x},{y}); got {:?}",
                &out[off..off + 4],
            );
        }
        for x in 4..8u32 {
            let off = (y * 8 + x as usize) * 4;
            assert_eq!(
                &out[off..off + 4],
                &[0xFF, 0x00, 0x00, 0xFF],
                "right half should stay blue at ({x},{y}) — picture clip honoured; got {:?}",
                &out[off..off + 4],
            );
        }
    }
}
