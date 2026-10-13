//! Glyph pixel sources for the `CompositeGlyphs` path and their
//! conversion to the dense A8 bitmap the atlas uploader consumes.
//!
//! The backend's wire parse forwards a glyph's pixels *as stored in
//! the glyphset* — dense A8 for `PICT_a8`, the raw A1 wire bitmap
//! for `PICT_a1`, the raw ARGB32 wire bytes for `PICT_a8r8g8b8`.
//! Conversion to A8 is **deferred to the engine's atlas-miss
//! branch** via [`GlyphPixels::to_a8`]: a glyph already resident in
//! the GPU atlas is never converted again, mirroring Xorg/EXA's
//! convert-on-upload (`exa/exa_glyphs.c`). See
//! `docs/superpowers/findings/2026-07-08-xorg-render-optimization-gaps.md` #2.

use std::borrow::Cow;

use crate::kms::vk::glyph::GlyphLayout;

/// Pixel source for one glyph, as stored in its glyphset. Cheap to
/// carry (two `&[u8]` variants); the actual A1→A8 expansion only
/// happens on an atlas miss.
#[derive(Copy, Clone, Debug)]
pub enum GlyphPixels<'a> {
    /// Dense A8 coverage bitmap, row-major `w × h`. Native
    /// `PICT_a8` glyphs.
    A8(&'a [u8]),
    /// Raw A1 wire bitmap: rows padded to a 32-bit scanline unit,
    /// bit order = advertised `bitmap-bit-order` (LSBFirst on the
    /// common little-endian client). Expanded lazily by [`Self::to_a8`].
    A1Wire(&'a [u8]),
    /// Raw ARGB32 wire bytes, row-major `w × h` CARD32 pixels with
    /// no per-row padding (stride `4 * w`). **Memory order per pixel
    /// is `[B, G, R, A]`** — little-endian CARD32 with alpha at bits
    /// 24-31, so blue is the LOW byte. Reduced to a single A8
    /// coverage plane by [`Self::to_a8`] via
    /// [`reduce_argb32_glyph_to_a8_coverage`]; the four channels are
    /// kept intact this far so the component-alpha path can pack all
    /// of them (design
    /// `2026-09-10-component-alpha-glyphs-design.md`, stage 0).
    Argb32Wire(&'a [u8]),
}

/// The glyphset picture format a glyph is **stored** in — the
/// protocol fact, tagged onto every `CompositeGlyphInput` by the
/// backend's items parse.
///
/// Deliberately NOT a rendering decision. Which pipeline a glyph
/// ends up on is its effective [`GlyphLayout`]
/// (`crate::kms::vk::glyph::GlyphLayout`), derived in the engine by
/// `RenderEngine::effective_glyph_layout`: that mapping depends on
/// device state (`component_alpha_supported`) and on what the atlas
/// upload stores, neither of which the protocol layer should be
/// consulting. The parse answers "what did the client give us"; the
/// engine answers "what can this device do with it".
///
/// One variant per [`GlyphPixels`] variant, and the parse derives
/// both from the same `GlyphSetFormat` match, so the byte encoding
/// and the format tag cannot disagree.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GlyphSourceFormat {
    /// `PICT_a8` — one coverage byte per pixel.
    A8,
    /// `PICT_a1` — one coverage BIT per pixel, 32-bit padded rows.
    A1,
    /// `PICT_a8r8g8b8` — per-channel coverage (subpixel/LCD AA) or
    /// colour (emoji).
    Argb32,
}

impl<'a> GlyphPixels<'a> {
    /// The glyphset picture format these bytes came in.
    ///
    /// The mapping between the two enums is total and 1:1, so the
    /// pair of them on one glyph would be two encodings of one fact,
    /// free to disagree. This accessor is the single encoding:
    /// `CompositeGlyphInput` no longer carries a separate
    /// `source_format` field.
    pub(crate) fn source_format(self) -> GlyphSourceFormat {
        match self {
            GlyphPixels::A8(_) => GlyphSourceFormat::A8,
            GlyphPixels::A1Wire(_) => GlyphSourceFormat::A1,
            GlyphPixels::Argb32Wire(_) => GlyphSourceFormat::Argb32,
        }
    }

    /// Produce the exact bytes the atlas uploader stages for an entry
    /// of `layout`, at that layout's packed width:
    ///
    /// * [`GlyphLayout::A8`] — one dense `w × h` coverage plane, via
    ///   [`Self::to_a8`]. An `ARGB32` source is REDUCED to the mean of
    ///   its logical R, G, B here; that is the `dualSrcBlend`-less
    ///   fallback the device layer promises, and the only path an
    ///   ARGB32 glyph takes on such a device.
    /// * [`GlyphLayout::ComponentAlpha`] — FOUR horizontally adjacent
    ///   coverage planes (logical R, G, B, A) at packed width
    ///   `4 * w`, via
    ///   [`pack_argb32_glyph_to_component_alpha_planes`].
    ///
    /// Returns `None` if the source is too short for the declared
    /// dimensions, or if `ComponentAlpha` is asked of a source that
    /// is not `ARGB32` — the caller derives the layout from this
    /// glyph's own source format
    /// (`RenderEngine::effective_glyph_layout`), so that combination
    /// is a logic error, not client input, and staging a single-plane
    /// A8 glyph under a four-plane layout would sample three
    /// neighbours as its G, B and A coverage.
    pub(crate) fn to_atlas_bytes(
        self,
        w: u32,
        h: u32,
        layout: GlyphLayout,
    ) -> Option<Cow<'a, [u8]>> {
        match layout {
            GlyphLayout::A8 => self.to_a8(w, h),
            GlyphLayout::ComponentAlpha => {
                let GlyphPixels::Argb32Wire(src) = self else {
                    log::error!(
                        "glyph upload: component-alpha layout asked of a {:?} source; \
                         only ARGB32 carries four channels",
                        self.source_format(),
                    );
                    return None;
                };
                let cells = (w as usize).checked_mul(h as usize)?;
                let need = cells.checked_mul(4)?;
                if src.len() < need {
                    return None;
                }
                Some(Cow::Owned(pack_argb32_glyph_to_component_alpha_planes(
                    src, w, h,
                )))
            }
        }
    }

    /// Produce the dense `w × h` A8 bitmap the atlas uploader copies
    /// into its staging buffer. `A8` borrows in place (truncated to
    /// `w × h`); `A1Wire` expands and `Argb32Wire` reduces into an
    /// owned buffer. Returns `None` if the source is too short for
    /// the declared dimensions (caller drops the glyph), so a
    /// malformed request can never index out of bounds during
    /// upload.
    pub(crate) fn to_a8(self, w: u32, h: u32) -> Option<Cow<'a, [u8]>> {
        let cells = (w as usize).checked_mul(h as usize)?;
        match self {
            GlyphPixels::A8(src) => {
                if src.len() < cells {
                    return None;
                }
                Some(Cow::Borrowed(&src[..cells]))
            }
            GlyphPixels::A1Wire(src) => {
                let wire_stride = (w as usize).div_ceil(32) * 4;
                let need = wire_stride.checked_mul(h as usize)?;
                if src.len() < need {
                    return None;
                }
                Some(Cow::Owned(expand_a1_glyph_to_a8(src, w, h)))
            }
            GlyphPixels::Argb32Wire(src) => {
                let need = cells.checked_mul(4)?;
                if src.len() < need {
                    return None;
                }
                Some(Cow::Owned(reduce_argb32_glyph_to_a8_coverage(src, w, h)))
            }
        }
    }
}

/// Reduce an ARGB32 glyph's wire bytes to a single A8 coverage
/// plane, row-major `w × h` at **logical width `w`** — the mean of
/// logical R, G and B.
///
/// **Wire byte order is `[B, G, R, A]`** per pixel: X RENDER
/// `PICT_a8r8g8b8` is a little-endian CARD32 with alpha-shift 24, so
/// blue is the LOW byte of the four and alpha the HIGH one. Reading
/// bytes 1..=3 (i.e. `[G, R, A]`) instead of 0..=2 is the trap here:
/// it errors nothing and paints plausible-looking text off by a
/// channel.
///
/// **Coverage comes from R, G and B, never from the alpha byte.** A
/// subpixel-AA client puts one coverage value per LCD subpixel in R,
/// G and B and leaves alpha at 255 across the whole glyph box —
/// measured on OpenJDK 25, where a 9×9 glyph had exactly one
/// distinct alpha value (255, all 81 pixels) against 28 distinct
/// `max(rgb)` values. Taking the alpha byte therefore yields a solid
/// `0xFF` rectangle, which is precisely the "text renders as blocks"
/// defect this reduction fixes.
///
/// Pure function of the wire bytes, so it is testable without a GPU
/// and needs no runtime switch to reach
/// (`feedback_no_feature_kill_switches`).
///
/// **Narrowed, not replaced.** This is now the treatment of an ARGB32
/// glyphset on a device WITHOUT `dualSrcBlend` only, where
/// `vk/device.rs` already promises grayscale AA — `component_alpha`
/// is then unavailable, `RenderEngine::effective_glyph_layout` answers
/// `A8` for every source format, and this reduction is what makes the
/// letterforms legible instead of solid blocks. With `dualSrcBlend`
/// present the glyph is packed as four planes by
/// [`pack_argb32_glyph_to_component_alpha_planes`] instead and this
/// function is not called.
///
/// The mean is **symmetric in its arguments**, so no test of this
/// reduction can detect an R↔B transposition; only the packed path's
/// exact-texel assertions can. The mistake this one guards against is
/// reading bytes `1..=3` (including alpha, dropping blue).
///
/// Rows short of `4 * w * h` bytes are left zero rather than
/// panicking; the caller length-checks first, so this is belt and
/// braces.
pub fn reduce_argb32_glyph_to_a8_coverage(wire: &[u8], w: u32, h: u32) -> Vec<u8> {
    let w = w as usize;
    let h = h as usize;
    // Dense CARD32 rows: X RENDER AddGlyphs pads a glyph row to a
    // 4-byte boundary, which 4 bytes per pixel already satisfies.
    let stride = w * 4;
    let mut a8 = vec![0u8; w * h];
    for row in 0..h {
        let row_off = row * stride;
        if row_off + stride > wire.len() {
            break;
        }
        for col in 0..w {
            let px = row_off + col * 4;
            // [B, G, R, A] — blue is the low byte. See above.
            let b = u32::from(wire[px]);
            let g = u32::from(wire[px + 1]);
            let r = u32::from(wire[px + 2]);
            // Mean of the three logical colour channels, rounded to
            // nearest. The alpha byte at `px + 3` is deliberately
            // NOT read: it is 255 everywhere on a subpixel-AA glyph.
            a8[row * w + col] = u8::try_from((r + g + b + 1) / 3).unwrap_or(u8::MAX);
        }
    }
    a8
}

/// Pack an `ARGB32` glyph's wire bytes into **four horizontally
/// adjacent coverage planes** — logical R, G, B, A in that order,
/// each `w` texels wide — row-major at packed width `4 * w` and
/// height `h`.
///
/// This is the real component-alpha path: X RENDER gives a glyph
/// carrying both alpha and RGB per-channel coverage semantics
/// (Xorg's `NeedsComponent`), a subpixel-AA client puts one coverage
/// value per LCD subpixel in R, G and B, and the fragment shader
/// applies each as that channel's own coverage.
///
/// **Wire byte order is `[B, G, R, A]`** per pixel: `PICT_a8r8g8b8`
/// is a little-endian CARD32 with alpha-shift 24, so blue is the LOW
/// byte and alpha the HIGH one. **Packed texel order is logical R, G,
/// B, A** — i.e. plane 0 holds wire byte 2, plane 1 wire byte 1,
/// plane 2 wire byte 0, plane 3 wire byte 3. Both ends of that
/// mapping are stated deliberately: a channel swap here paints
/// plausible-looking text in the wrong colour and errors nothing
/// (design invariant 4), and unlike the grayscale reduction — whose
/// mean is symmetric and therefore blind to it — this is where an
/// R↔B transposition is detectable at all.
///
/// **Four planes, not three.** Java's alpha byte is 255 across the
/// whole glyph box, but the shader needs the glyph's own alpha for
/// the ALPHA channel's blend factor, and Xorg applies component-alpha
/// to every `ARGB32` glyphset rather than only to the ones whose
/// alpha happens to be constant. Dropping it would work for this JVM
/// and break on the next client (design invariant 3). A
/// component-alpha glyph therefore costs 4× its area in the atlas.
///
/// Layout, for `w = 3`:
///
/// ```text
///  row 0: R R R | G G G | B B B | A A A
///  row 1: R R R | G G G | B B B | A A A
/// ```
///
/// The four planes are distinct images that merely happen to be
/// adjacent, which is why the shader reaches them by `texelFetch` at
/// an explicit plane stride and never by interpolating a UV across
/// the packed run.
///
/// Rows short of `4 * w * h` bytes are left zero rather than
/// panicking; the caller length-checks first, so this is belt and
/// braces.
pub(crate) fn pack_argb32_glyph_to_component_alpha_planes(wire: &[u8], w: u32, h: u32) -> Vec<u8> {
    let w = w as usize;
    let h = h as usize;
    // Dense CARD32 rows: X RENDER AddGlyphs pads a glyph row to a
    // 4-byte boundary, which 4 bytes per pixel already satisfies.
    let src_stride = w * 4;
    let packed_w = w * PLANES as usize;
    let mut out = vec![0u8; packed_w * h];
    for row in 0..h {
        let row_off = row * src_stride;
        if row_off + src_stride > wire.len() {
            break;
        }
        let out_row = row * packed_w;
        for col in 0..w {
            let px = row_off + col * 4;
            // Wire [B, G, R, A] → packed planes [R, G, B, A].
            let b = wire[px];
            let g = wire[px + 1];
            let r = wire[px + 2];
            let a = wire[px + 3];
            out[out_row + col] = r;
            out[out_row + w + col] = g;
            out[out_row + 2 * w + col] = b;
            out[out_row + 3 * w + col] = a;
        }
    }
    out
}

/// Coverage planes a `ComponentAlpha` atlas entry occupies: logical
/// R, G, B, A. The atlas footprint of such a glyph is
/// `PLANES * logical_w` texels wide.
pub(crate) const PLANES: u32 = 4;

/// Expand a wire A1 glyph bitmap to dense A8 (`0x00` / `0xFF` per
/// pixel), row-major `w × h`.
///
/// Wire rows are padded to a 32-bit scanline unit; bit order is
/// LSBFirst (bit 0 of a byte is the leftmost of its 8-pixel group),
/// matching yserver's advertised `bitmap-bit-order` on x86. Reading
/// MSBFirst would mirror every 8-pixel run — the issue-#77 bitmap-font
/// regression the tests below guard.
pub(crate) fn expand_a1_glyph_to_a8(pixels: &[u8], gw: u32, gh: u32) -> Vec<u8> {
    let wire_stride = (gw as usize).div_ceil(32) * 4;
    let mut a8 = vec![0u8; (gw as usize) * (gh as usize)];
    for row in 0..(gh as usize) {
        let src_off = row * wire_stride;
        if src_off + wire_stride > pixels.len() {
            break;
        }
        for col in 0..(gw as usize) {
            let byte = pixels[src_off + col / 8];
            let bit = (byte >> (col & 7)) & 1;
            a8[row * (gw as usize) + col] = if bit != 0 { 0xFF } else { 0 };
        }
    }
    a8
}

#[cfg(test)]
mod tests {
    use super::{
        GlyphPixels, PLANES, expand_a1_glyph_to_a8, pack_argb32_glyph_to_component_alpha_planes,
        reduce_argb32_glyph_to_a8_coverage,
    };
    use crate::kms::vk::glyph::GlyphLayout;
    use std::borrow::Cow;

    #[test]
    fn to_a8_borrows_dense_a8_truncated_to_cells() {
        // A8 source longer than w*h (trailing padding bytes) borrows
        // in place, truncated to exactly the glyph cells — no copy.
        let src = [10u8, 20, 30, 40, 0xEE, 0xEE];
        let out = GlyphPixels::A8(&src).to_a8(2, 2).expect("fits");
        assert!(matches!(out, Cow::Borrowed(_)), "A8 must not copy");
        assert_eq!(&*out, &[10, 20, 30, 40]);
    }

    #[test]
    fn to_a8_expands_a1_wire_lsb_first() {
        // 8x1 wire byte 0b0000_0001 → col 0 set (LSBFirst). Owned copy.
        let wire = [0b0000_0001u8, 0, 0, 0];
        let out = GlyphPixels::A1Wire(&wire).to_a8(8, 1).expect("fits");
        assert!(matches!(out, Cow::Owned(_)), "A1 must expand into owned");
        assert_eq!(&*out, &[0xFF, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn to_a8_none_when_a8_too_short() {
        let src = [1u8, 2, 3];
        assert!(GlyphPixels::A8(&src).to_a8(2, 2).is_none());
    }

    #[test]
    fn to_a8_none_when_a1_wire_too_short() {
        // 8x2 needs two 4-byte scanline units; give only one.
        let wire = [0xFFu8, 0, 0, 0];
        assert!(GlyphPixels::A1Wire(&wire).to_a8(8, 2).is_none());
    }

    #[test]
    fn to_a8_none_when_argb32_wire_too_short() {
        // 2x2 needs 4 pixels × 4 bytes = 16; give 15.
        let wire = [0u8; 15];
        assert!(GlyphPixels::Argb32Wire(&wire).to_a8(2, 2).is_none());
    }

    // ── the ARGB32 → A8 coverage reduction ──
    // Subpixel/LCD-antialiased text used to render as one solid block
    // per glyph because ingest kept the ARGB32 **alpha** byte as the
    // coverage plane, and a subpixel-AA client leaves alpha at 255
    // across the whole glyph box.

    #[test]
    fn argb32_reduction_is_the_mean_of_logical_rgb_at_exact_values() {
        // Wire memory order is [B, G, R, A]: blue LOW, alpha HIGH.
        //
        // px0: B=0x20 G=0x60 R=0xA0 A=0xE0 → (0xA0+0x60+0x20)/3 = 96.
        // px1: B=0x10 G=0x40 R=0x50 A=0xFF → (0x50+0x40+0x10+1)/3 = 53.
        //
        // Exact values, not a "the channels differ" check: that would
        // pass with red and blue reversed. The two pixels together
        // also pin the reduction against the near misses —
        //   * reading bytes 1..=3 as if they were R,G,B (i.e. [G,R,A])
        //     gives 160 and 133, not 96 and 53;
        //   * taking the alpha byte gives 0xE0 and 0xFF;
        //   * taking any single colour channel gives one of the
        //     inputs — px1's mean 53 is not 0x10, 0x40 or 0x50, and
        //     px0's 96 rules out R and B.
        let wire = [0x20u8, 0x60, 0xA0, 0xE0, 0x10, 0x40, 0x50, 0xFF];
        let a8 = reduce_argb32_glyph_to_a8_coverage(&wire, 2, 1);
        assert_eq!(
            a8,
            vec![96u8, 53u8],
            "coverage must be the mean of logical R, G and B read from \
             wire bytes [B, G, R, A]"
        );

        // Same answer through the uploader's entry point, so the
        // engine's atlas-miss branch cannot be wired to a different
        // reduction than the one tested here.
        let via_to_a8 = GlyphPixels::Argb32Wire(&wire).to_a8(2, 1).expect("fits");
        assert!(
            matches!(via_to_a8, Cow::Owned(_)),
            "ARGB32 must reduce into an owned buffer"
        );
        assert_eq!(&*via_to_a8, &[96u8, 53u8]);
    }

    #[test]
    fn argb32_reduction_of_a_subpixel_aa_glyph_is_not_a_solid_block() {
        // Synthetic 9×9 glyph reproducing the *measured* signature of
        // OpenJDK 25's LCD glyphs (one vng run, temporary
        // instrumentation): alpha has exactly ONE distinct value, 255
        // over all 81 pixels, while max(R,G,B) spans dozens. These
        // are not the literal Java bytes — the instrumentation
        // recorded histograms, not the glyph — but the property that
        // makes the old code fail is the whole fixture.
        const W: usize = 9;
        const H: usize = 9;
        let mut wire = vec![0u8; W * H * 4];
        for row in 0..H {
            for col in 0..W {
                let px = (row * W + col) * 4;
                wire[px] = u8::try_from((row * 29 + col * 7) % 256).expect("masked");
                wire[px + 1] = u8::try_from((row * 13 + col * 23 + 5) % 256).expect("masked");
                wire[px + 2] = u8::try_from((row * 41 + col * 3 + 11) % 256).expect("masked");
                wire[px + 3] = 0xFF;
            }
        }

        // The fixture really does have the measured shape.
        let mut alphas: Vec<u8> = wire.iter().skip(3).step_by(4).copied().collect();
        alphas.dedup();
        assert_eq!(alphas, vec![0xFFu8], "fixture: alpha constant 255");
        let mut maxima: Vec<u8> = (0..W * H)
            .map(|i| wire[i * 4].max(wire[i * 4 + 1]).max(wire[i * 4 + 2]))
            .collect();
        maxima.sort_unstable();
        maxima.dedup();
        assert!(
            maxima.len() >= 28,
            "fixture: max(rgb) must span the measured spread; got {}",
            maxima.len()
        );

        let a8 = reduce_argb32_glyph_to_a8_coverage(&wire, 9, 9);
        assert_eq!(a8.len(), W * H);

        // The regression guard. Reading the alpha byte for coverage
        // yields 0xFF everywhere, i.e. a solid block — exactly the
        // reported defect, and invisible to any assertion weaker than
        // this one.
        assert!(
            a8.iter().any(|&v| v != a8[0]),
            "reduced coverage must vary across the glyph; a constant \
             plane means the alpha byte was read instead of R, G, B"
        );
        assert!(
            a8.iter().any(|&v| v != 0xFF),
            "reduced coverage must not be a solid 0xFF block"
        );

        // Exact spot checks, so the mapping is pinned and not merely
        // shown to be non-constant.
        assert_eq!(a8[0], 5, "(0,0): B=0x00 G=0x05 R=0x0B → 5");
        assert_eq!(a8[4 * W + 4], 160, "(4,4): B=0x90 G=0xA1 R=0xAF → 160");
        assert_eq!(a8[8 * W + 8], 59, "(8,8)");
    }

    #[test]
    fn a8_and_a1_glyphs_are_untouched_by_the_argb32_reduction() {
        // Invariant 1 of the design: an A8 or A1 glyphset must be
        // byte-identical before and after. That is the overwhelmingly
        // common path. Asserted here at the dispatch that gained the
        // new variant.
        let a8_src = [0x00u8, 0x7F, 0xFF, 0x40, 0xEE, 0xEE];
        let out = GlyphPixels::A8(&a8_src).to_a8(2, 2).expect("fits");
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "A8 must still borrow in place — no reduction, no copy"
        );
        assert_eq!(
            &*out,
            &a8_src[..4],
            "A8 coverage bytes must pass through unchanged"
        );

        let a1_wire = [0b0101_0011u8, 0, 0, 0];
        let out = GlyphPixels::A1Wire(&a1_wire).to_a8(8, 1).expect("fits");
        assert_eq!(
            &*out,
            &expand_a1_glyph_to_a8(&a1_wire, 8, 1)[..],
            "A1 must still take the LSBFirst expansion, unchanged"
        );
        assert_eq!(&*out, &[0xFF, 0xFF, 0, 0, 0xFF, 0, 0xFF, 0]);
    }

    // ── the ARGB32 → four-plane component-alpha pack ──
    // The real subpixel path. Unlike the grayscale reduction above —
    // whose mean is symmetric and therefore blind to an R↔B
    // transposition — these are the assertions where the channel
    // mapping is detectable at all.

    #[test]
    fn argb32_packs_four_planes_in_logical_rgba_order_at_exact_values() {
        // ONE pixel, four mutually distinct channel values, so every
        // near miss lands somewhere visible:
        //   wire  [B, G, R, A] = [0x20, 0x60, 0xA0, 0xE0]
        //   packed plane order   [R, G, B, A] = [0xA0, 0x60, 0x20, 0xE0]
        //
        // Deliberately not an `r != g != b` check: that passes with
        // red and blue reversed, which is the single most likely
        // mistake here.
        let wire = [0x20u8, 0x60, 0xA0, 0xE0];
        let packed = pack_argb32_glyph_to_component_alpha_planes(&wire, 1, 1);
        assert_eq!(
            packed,
            vec![0xA0u8, 0x60, 0x20, 0xE0],
            "packed texels must be logical R, G, B, A read from wire [B, G, R, A]",
        );
        // The three ways to get it wrong, spelled out so the failure
        // message names them:
        //   * R/B swapped        → [0x20, 0x60, 0xA0, 0xE0]
        //   * planes in wire order → [0x20, 0x60, 0xA0, 0xE0] (same)
        //   * alpha dropped, RGB only → length 3
        assert_ne!(
            packed,
            vec![0x20u8, 0x60, 0xA0, 0xE0],
            "packing the wire bytes in place leaves blue in the R plane",
        );
        assert_eq!(packed.len(), 4, "four planes, not three: alpha is used");
    }

    #[test]
    fn argb32_packs_planes_as_adjacent_texel_runs_at_four_times_the_width() {
        // 3×2 glyph, every pixel of a row distinct, so the row layout
        // is pinned: R R R | G G G | B B B | A A A, contiguous, at
        // packed width 4 * 3 = 12.
        const W: usize = 3;
        const H: usize = 2;
        let mut wire = vec![0u8; W * H * 4];
        for row in 0..H {
            for col in 0..W {
                let px = (row * W + col) * 4;
                let base = u8::try_from(row * 0x40 + col * 0x10).expect("small");
                wire[px] = base; // B
                wire[px + 1] = base + 1; // G
                wire[px + 2] = base + 2; // R
                wire[px + 3] = base + 3; // A
            }
        }

        let packed = pack_argb32_glyph_to_component_alpha_planes(&wire, 3, 2);
        let packed_w = W * PLANES as usize;
        assert_eq!(packed.len(), packed_w * H);

        for row in 0..H {
            for col in 0..W {
                let base = u8::try_from(row * 0x40 + col * 0x10).expect("small");
                let at = |plane: usize| packed[row * packed_w + plane * W + col];
                assert_eq!(at(0), base + 2, "({col},{row}) plane 0 is logical R");
                assert_eq!(at(1), base + 1, "({col},{row}) plane 1 is logical G");
                assert_eq!(at(2), base, "({col},{row}) plane 2 is logical B");
                assert_eq!(at(3), base + 3, "({col},{row}) plane 3 is logical A");
            }
        }

        // Row 1's planes must not have landed in row 0's tail: a
        // packer that wrote at the LOGICAL width would put row 1's R
        // plane where row 0's G plane belongs.
        assert_eq!(
            packed[0..packed_w],
            [
                0x02, 0x12, 0x22, 0x01, 0x11, 0x21, 0x00, 0x10, 0x20, 0x03, 0x13, 0x23
            ],
            "row 0 must be four contiguous 3-texel plane runs",
        );
    }

    #[test]
    fn argb32_pack_preserves_a_measured_subpixel_glyph_alpha_and_colour_split() {
        // The measured OpenJDK 25 signature: alpha constant 255,
        // max(rgb) spanning dozens of values. The pack must keep BOTH
        // — the colour planes vary and the alpha plane does not — so
        // a regression that fed the alpha byte into the colour planes
        // (the original defect) shows up as a constant colour plane.
        const W: usize = 9;
        const H: usize = 9;
        let mut wire = vec![0u8; W * H * 4];
        for row in 0..H {
            for col in 0..W {
                let px = (row * W + col) * 4;
                wire[px] = u8::try_from((row * 29 + col * 7) % 256).expect("masked");
                wire[px + 1] = u8::try_from((row * 13 + col * 23 + 5) % 256).expect("masked");
                wire[px + 2] = u8::try_from((row * 41 + col * 3 + 11) % 256).expect("masked");
                wire[px + 3] = 0xFF;
            }
        }

        let packed = pack_argb32_glyph_to_component_alpha_planes(&wire, 9, 9);
        let packed_w = W * PLANES as usize;
        let plane = |k: usize| -> Vec<u8> {
            (0..H)
                .flat_map(|row| {
                    packed[row * packed_w + k * W..row * packed_w + (k + 1) * W].to_vec()
                })
                .collect()
        };

        for k in 0..3 {
            let p = plane(k);
            assert!(
                p.iter().any(|&v| v != p[0]),
                "colour plane {k} is constant — the alpha byte was packed into it",
            );
        }
        assert!(
            plane(3).iter().all(|&v| v == 0xFF),
            "the alpha plane must be the glyph's own alpha, 255 here",
        );
        // Exact spot checks against the wire, so the mapping is
        // pinned and not merely shown to be non-constant.
        let at = |row: usize, col: usize, k: usize| packed[row * packed_w + k * W + col];
        assert_eq!(at(4, 4, 0), wire[(4 * W + 4) * 4 + 2], "R plane");
        assert_eq!(at(4, 4, 1), wire[(4 * W + 4) * 4 + 1], "G plane");
        assert_eq!(at(4, 4, 2), wire[(4 * W + 4) * 4], "B plane");
    }

    /// The upload entry point routes on the LAYOUT, and the two
    /// layouts produce different byte counts from the same source —
    /// which is what makes confusing `packed_w` with `logical_w`
    /// reachable at all.
    #[test]
    fn to_atlas_bytes_routes_a8_to_the_reduction_and_component_alpha_to_the_pack() {
        let wire = [0x20u8, 0x60, 0xA0, 0xE0, 0x10, 0x40, 0x50, 0xFF];
        let src = GlyphPixels::Argb32Wire(&wire);

        // `A8` — the no-`dualSrcBlend` fallback. One plane at the
        // LOGICAL width, the mean of logical R, G, B.
        let a8 = src.to_atlas_bytes(2, 1, GlyphLayout::A8).expect("fits");
        assert_eq!(&*a8, &[96u8, 53u8], "A8 layout must take the reduction");

        // `ComponentAlpha` — four planes at 4 * w.
        let ca = src
            .to_atlas_bytes(2, 1, GlyphLayout::ComponentAlpha)
            .expect("fits");
        assert_eq!(
            &*ca,
            &[0xA0u8, 0x50, 0x60, 0x40, 0x20, 0x10, 0xE0, 0xFF],
            "ComponentAlpha layout must take the four-plane pack",
        );
        assert_eq!(
            ca.len(),
            4 * a8.len(),
            "a component-alpha glyph is 4x the area"
        );

        // Short source: refused on both layouts rather than indexing
        // out of bounds.
        let short = GlyphPixels::Argb32Wire(&wire[..7]);
        assert!(short.to_atlas_bytes(2, 1, GlyphLayout::A8).is_none());
        assert!(
            short
                .to_atlas_bytes(2, 1, GlyphLayout::ComponentAlpha)
                .is_none()
        );
    }

    /// An A8 or A1 source can never be staged under a four-plane
    /// layout: it has no G, B or A channel, and three neighbours
    /// would be sampled as them. The caller derives the layout from
    /// this glyph's own source format, so this is a logic error
    /// caught here rather than a wrong picture.
    #[test]
    fn to_atlas_bytes_refuses_component_alpha_for_a_single_channel_source() {
        let a8 = [0x10u8, 0x20, 0x30, 0x40];
        assert!(
            GlyphPixels::A8(&a8)
                .to_atlas_bytes(2, 2, GlyphLayout::ComponentAlpha)
                .is_none(),
        );
        let a1 = [0b0000_0001u8, 0, 0, 0];
        assert!(
            GlyphPixels::A1Wire(&a1)
                .to_atlas_bytes(8, 1, GlyphLayout::ComponentAlpha)
                .is_none(),
        );
        // And they still work under the layout they DO have.
        assert!(
            GlyphPixels::A8(&a8)
                .to_atlas_bytes(2, 2, GlyphLayout::A8)
                .is_some(),
        );
    }

    /// The format tag is derived from the byte encoding, not carried
    /// beside it — the simplification that removed
    /// `CompositeGlyphInput.source_format`. Total and 1:1, asserted
    /// so a new variant cannot be added without answering here.
    #[test]
    fn source_format_is_one_to_one_with_the_pixel_variant() {
        use super::GlyphSourceFormat;
        let bytes = [0u8; 8];
        assert_eq!(
            GlyphPixels::A8(&bytes).source_format(),
            GlyphSourceFormat::A8
        );
        assert_eq!(
            GlyphPixels::A1Wire(&bytes).source_format(),
            GlyphSourceFormat::A1
        );
        assert_eq!(
            GlyphPixels::Argb32Wire(&bytes).source_format(),
            GlyphSourceFormat::Argb32
        );
    }

    // ── issue #77: bitmap fonts (terminus) rendered backwards ──
    // Guards the LSBFirst expansion order at its new home.

    #[test]
    fn a1_glyph_expands_lsb_first_not_mirrored() {
        // 8x1 glyph, one wire byte (padded to a 32-bit unit). Byte
        // 0b0000_0001 has only bit 0 set → LSBFirst means the LEFTMOST
        // pixel (col 0) is on and the rest are off.
        let wire = [0b0000_0001u8, 0, 0, 0];
        let a8 = expand_a1_glyph_to_a8(&wire, 8, 1);
        assert_eq!(
            a8,
            vec![0xFF, 0, 0, 0, 0, 0, 0, 0],
            "col 0 must be the set pixel (LSBFirst); an MSBFirst read \
             would light col 7 instead, mirroring the glyph"
        );

        // A left-to-right ramp: bits 0,1,2 set (cols 0,1,2), 0b0000_0111.
        let wire = [0b0000_0111u8, 0, 0, 0];
        let a8 = expand_a1_glyph_to_a8(&wire, 8, 1);
        assert_eq!(a8, vec![0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a1_glyph_expands_wide_glyph_across_scanline_units() {
        // 12px-wide glyph: still one 32-bit scanline unit (4 bytes) per
        // row, spanning two bytes of pixels. Verify column indexing is
        // continuous and LSBFirst across the byte boundary.
        // Byte 0 = 0b1000_0001 → cols 0 and 7 set.
        // Byte 1 = 0b0000_1000 → col 8+3 = 11 set.
        let wire = [0b1000_0001u8, 0b0000_1000u8, 0, 0];
        let a8 = expand_a1_glyph_to_a8(&wire, 12, 1);
        let mut expect = vec![0u8; 12];
        expect[0] = 0xFF;
        expect[7] = 0xFF;
        expect[11] = 0xFF;
        assert_eq!(a8, expect);
    }
}
