use super::*;

// ── CopyArea joint src/dst clamp (negative-offset smear fix) ──
// Pure arithmetic; no Vk. Expected values are grounded in the X11
// CopyArea spec: copy the full sub-rect overlap that survives
// clamping to BOTH drawables, keeping src↔dst aligned. Regression
// guard for the MATE compositor slow-drag-left shadow smear
// (docs/superpowers/findings/2026-07-08-mate-compositor-drag-smear-diagnosis.md).
mod clamp_copy {
    use super::super::clamp_copy_rects;
    use ash::vk;

    fn ext(w: u32, h: u32) -> vk::Extent2D {
        vk::Extent2D {
            width: w,
            height: h,
        }
    }
    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: ext(w, h),
        }
    }

    // The bug: a present/CopyArea whose update rect starts 10px off
    // the top-left (dst_pos == src offset == -10, as in the live
    // MATE trace) must still copy the full 90px in-bounds overlap —
    // NOT 80 (the double-subtracted value). src and dst both skip
    // the 10 off-screen columns/rows and stay aligned at origin 0.
    #[test]
    fn negative_aligned_offset_copies_full_overlap() {
        let (src, dst) = clamp_copy_rects(
            rect(-10, -10, 100, 100),
            vk::Offset2D { x: -10, y: -10 },
            ext(2560, 1440),
            ext(2560, 1440),
        )
        .expect("visible overlap");
        assert_eq!(
            dst.extent,
            ext(90, 90),
            "dst extent (was 80 with the double-subtract bug)"
        );
        assert_eq!(src.extent, ext(90, 90), "src extent must match dst extent");
        assert_eq!(dst.offset, vk::Offset2D { x: 0, y: 0 });
        assert_eq!(src.offset, vk::Offset2D { x: 0, y: 0 });
    }

    // General case the old inline code also got wrong: dst_pos
    // negative but src offset 0 → the off-screen dst columns must
    // advance the SOURCE origin so the copy stays aligned.
    #[test]
    fn negative_dst_advances_src_origin() {
        let (src, dst) = clamp_copy_rects(
            rect(0, 0, 100, 50),
            vk::Offset2D { x: -10, y: 0 },
            ext(2560, 1440),
            ext(2560, 1440),
        )
        .expect("visible overlap");
        assert_eq!(dst.offset, vk::Offset2D { x: 0, y: 0 });
        assert_eq!(src.offset, vk::Offset2D { x: 10, y: 0 });
        assert_eq!(dst.extent, ext(90, 50));
        assert_eq!(src.extent, ext(90, 50));
    }

    #[test]
    fn positive_in_bounds_unchanged() {
        let (src, dst) = clamp_copy_rects(
            rect(100, 50, 200, 100),
            vk::Offset2D { x: 100, y: 50 },
            ext(2560, 1440),
            ext(2560, 1440),
        )
        .expect("visible overlap");
        assert_eq!(src.offset, vk::Offset2D { x: 100, y: 50 });
        assert_eq!(dst.offset, vk::Offset2D { x: 100, y: 50 });
        assert_eq!(src.extent, ext(200, 100));
        assert_eq!(dst.extent, ext(200, 100));
    }

    #[test]
    fn overflow_right_bottom_clamps() {
        let (_src, dst) = clamp_copy_rects(
            rect(2500, 1400, 100, 100),
            vk::Offset2D { x: 2500, y: 1400 },
            ext(2560, 1440),
            ext(2560, 1440),
        )
        .expect("visible overlap");
        assert_eq!(dst.extent, ext(60, 40));
    }

    #[test]
    fn fully_offscreen_left_returns_none() {
        assert!(
            clamp_copy_rects(
                rect(-200, 0, 100, 100),
                vk::Offset2D { x: -200, y: 0 },
                ext(2560, 1440),
                ext(2560, 1440)
            )
            .is_none()
        );
    }
}

// ── #137 tier 1: which drawable glyph sources are admissible ──
// Pure predicate, no Vk. Every assertion here is a claim about
// exactness: an admitted source must be one whose sampled value is
// the SAME for every destination pixel, because the glyph path
// collapses it to one colour.
mod uniform_glyph_source {
    use super::super::{DrawableId, SourceDrawable, uniform_pixel_glyph_source};
    use crate::kms::cpu_types::Repeat;
    use ash::vk;

    fn ext(w: u32, h: u32) -> vk::Extent2D {
        vk::Extent2D {
            width: w,
            height: h,
        }
    }

    const REPEATS: [Repeat; 3] = [Repeat::Normal, Repeat::Pad, Repeat::Reflect];

    /// The cross product the plan asks for: only a 1x1 sampled
    /// domain, only a plane-covering repeat, only `mask_format 0`.
    #[test]
    fn admits_exactly_one_pixel_under_a_covering_repeat() {
        for (w, h) in [(1u32, 1u32), (1, 2), (2, 1), (16, 16)] {
            let src = SourceDrawable::whole(DrawableId::for_tests(1));
            let storage = ext(w, h);
            let one_pixel = w == 1 && h == 1;
            for repeat in REPEATS {
                let got = uniform_pixel_glyph_source(src, repeat, storage, 0);
                assert_eq!(
                    got.is_some(),
                    one_pixel,
                    "storage {w}x{h} under {repeat:?} at mask_format 0"
                );
            }
            // RepeatNone reads transparent outside the pixel, so it
            // is NOT one colour over the plane even at 1x1.
            assert!(
                uniform_pixel_glyph_source(src, Repeat::None, storage, 0).is_none(),
                "storage {w}x{h} under RepeatNone"
            );
            // mask_format != 0 selects Xorg's accumulate-into-a-mask
            // branch, which we do not implement; keep dropping.
            for repeat in REPEATS {
                assert!(
                    uniform_pixel_glyph_source(src, repeat, storage, 0x21).is_none(),
                    "storage {w}x{h} under {repeat:?} at a non-zero mask_format"
                );
            }
        }
    }

    /// The redirected-window shape, and the one a pixmap-only test
    /// would pass while leaving broken: a 1x1 CONTENT domain inside
    /// a large backing is admissible, and the read rect is at the
    /// content OFFSET, not the backing origin.
    #[test]
    fn a_one_pixel_content_domain_inside_a_large_backing_is_admitted_at_its_offset() {
        let src = SourceDrawable::content(DrawableId::for_tests(2), (7, 9), ext(1, 1));
        let rect = uniform_pixel_glyph_source(src, Repeat::Normal, ext(640, 480), 0)
            .expect("a 1x1 content domain is admissible whatever the storage size");
        assert_eq!(rect.offset.x, 7);
        assert_eq!(rect.offset.y, 9);
        assert_eq!(rect.extent, ext(1, 1));
    }

    /// The converse: a large content domain inside a 1x1-looking
    /// read is not admissible. The domain wins over the storage in
    /// both directions.
    #[test]
    fn a_larger_content_domain_drops_even_when_the_storage_is_small() {
        let src = SourceDrawable::content(DrawableId::for_tests(3), (0, 0), ext(4, 4));
        assert!(uniform_pixel_glyph_source(src, Repeat::Normal, ext(1, 1), 0).is_none());
    }

    /// `whole()` over a 1x1 storage — Java's `XRSolidSrcPict` — is
    /// admitted and read at the origin.
    #[test]
    fn a_one_by_one_pixmap_is_admitted_at_the_origin() {
        let src = SourceDrawable::whole(DrawableId::for_tests(4));
        for repeat in REPEATS {
            let rect = uniform_pixel_glyph_source(src, repeat, ext(1, 1), 0)
                .expect("1x1 storage sampled whole");
            assert_eq!(rect.offset.x, 0);
            assert_eq!(rect.offset.y, 0);
        }
    }

    /// `Src::server_internal` is unclipped and `get_image` clamps,
    /// so an offset outside the allocation would read nothing at
    /// all. Drop rather than read out of range.
    #[test]
    fn an_offset_outside_the_storage_drops() {
        let id = DrawableId::for_tests(5);
        for offset in [(64, 0), (0, 64), (-1, 0), (0, -1)] {
            let src = SourceDrawable::content(id, offset, ext(1, 1));
            assert!(
                uniform_pixel_glyph_source(src, Repeat::Normal, ext(64, 64), 0).is_none(),
                "offset {offset:?} is outside a 64x64 storage"
            );
        }
    }
}

// ── #137 tier 1: one wire pixel -> a premultiplied colour ──
// The two conversions in this function are silent when wrong, so
// they are pinned here rather than reasoned about: a channel swap
// paints the wrong colour and a wrong alpha paints nothing.
// Deliberately non-grey and non-symmetric values throughout, so no
// assertion can pass under a swap.
mod premul_from_wire {
    use super::super::premul_from_wire_pixel;
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    /// `get_image` hands back wire order, blue in the low byte;
    /// `ResolvedSource::Solid` is logical `[R, G, B, A]`.
    #[test]
    fn depth_32_keeps_the_stored_alpha_and_reorders_to_rgba() {
        // Wire [B, G, R, A] = 0x11 blue, 0x22 green, 0x33 red,
        // 0x44 alpha.
        let got = premul_from_wire_pixel(&[0x11, 0x22, 0x33, 0x44], 32, RENDER_FMT_ARGB32)
            .expect("depth 32 is supported");
        let want = [
            0x33 as f32 / 255.0,
            0x22 as f32 / 255.0,
            0x11 as f32 / 255.0,
            0x44 as f32 / 255.0,
        ];
        assert_eq!(got, want, "expected [R, G, B, A] from wire [B, G, R, A]");
    }

    /// A depth-24 source has no alpha. Taking the stored byte would
    /// give `a = 0.0` and paint nothing -- the same symptom as the
    /// bug being fixed.
    #[test]
    fn depth_24_forces_opaque_whatever_the_stored_byte_says() {
        for stored_alpha in [0x00u8, 0x7f, 0xff] {
            let got = premul_from_wire_pixel(&[0x11, 0x22, 0x33, stored_alpha], 24, 0)
                .expect("depth 24 is supported");
            assert!(
                (got[3] - 1.0).abs() < f32::EPSILON,
                "depth 24 with stored alpha {stored_alpha:#x} must read a = 1.0, got {}",
                got[3],
            );
            assert_eq!(got[0], 0x33 as f32 / 255.0);
            assert_eq!(got[2], 0x11 as f32 / 255.0);
        }
    }

    /// A depth-32 storage wrapped by an `xRGB32` picture declares
    /// its alpha byte to be padding; the sampler pins alpha to ONE
    /// for it, so this must too.
    #[test]
    fn an_alphaless_pict_format_forces_opaque_at_depth_32() {
        for fmt in [RENDER_FMT_XRGB32, RENDER_FMT_RGB24] {
            let got = premul_from_wire_pixel(&[0x11, 0x22, 0x33, 0x00], 32, fmt)
                .expect("depth 32 is supported");
            assert!(
                (got[3] - 1.0).abs() < f32::EPSILON,
                "pict_format {fmt} declares no alpha, so a = 1.0"
            );
        }
    }

    /// A8 is alpha-only, matching the `AlphaOnlyR8` swizzle's
    /// `(0, 0, 0, R)`. Premultiplied, a pure-alpha picture's colour
    /// channels are zero.
    #[test]
    fn depth_8_is_alpha_only() {
        let got = premul_from_wire_pixel(&[0x40, 0, 0, 0], 8, 0).expect("A8 is supported");
        assert_eq!(got, [0.0, 0.0, 0.0, 0x40 as f32 / 255.0]);
    }

    /// A1 arrives bit-packed, LSB first.
    #[test]
    fn depth_1_reads_bit_zero() {
        assert_eq!(
            premul_from_wire_pixel(&[0x01, 0, 0, 0], 1, 0).expect("A1 is supported"),
            [0.0, 0.0, 0.0, 1.0]
        );
        // Bit 0 clear, higher bits set: pixel 0 is still zero.
        assert_eq!(
            premul_from_wire_pixel(&[0xfe, 0, 0, 0], 1, 0).expect("A1 is supported"),
            [0.0, 0.0, 0.0, 0.0]
        );
    }

    /// A short read (a clamped-away rect) and a depth with no
    /// RENDER format must both decline rather than index past the
    /// buffer or invent a colour.
    #[test]
    fn a_short_buffer_or_an_unsupported_depth_declines() {
        assert!(premul_from_wire_pixel(&[], 32, RENDER_FMT_ARGB32).is_none());
        assert!(premul_from_wire_pixel(&[0x11, 0x22, 0x33], 32, RENDER_FMT_ARGB32).is_none());
        assert!(premul_from_wire_pixel(&[], 8, 0).is_none());
        assert!(premul_from_wire_pixel(&[0x11, 0x22, 0x33, 0x44], 4, 0).is_none());
        assert!(premul_from_wire_pixel(&[0x11, 0x22, 0x33, 0x44], 16, 0).is_none());
    }
}

// ── frame-builder coalescing accounting (Slice-1 telemetry) ──
// Tests the pure run/session fold directly; the `RecordedOp` →
// `CoalesceClass` mapping is trivial field access exercised by the
// live path.
mod coalescing {
    use super::super::{CoalesceClass, CoalesceCounts, DrawableId, coalescing_counts};

    fn d(n: u64) -> DrawableId {
        DrawableId::for_tests(n)
    }
    /// Fold-clean composite to dst `n`.
    fn comp(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: false,
            folder_clean: true,
            dirty_clear_only: false,
        }
    }
    /// Composite to dst `n` blocked only by a solid clear — opens a
    /// session but cannot fold as a follower (Slice-1.5 prize).
    fn comp_clear(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: false,
            folder_clean: false,
            dirty_clear_only: true,
        }
    }
    /// Composite to dst `n` that reads dst via readback scratch —
    /// neither fold-clean nor clear-only (cross-kind bucket).
    fn comp_readback(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: false,
            folder_clean: false,
            dirty_clear_only: false,
        }
    }
    fn comp_self(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: true,
            folder_clean: false,
            dirty_clear_only: false,
        }
    }
    fn glyph(n: u64) -> CoalesceClass {
        CoalesceClass::PassNonComposite {
            dst: Some(d(n)),
            is_fill_or_logic: false,
        }
    }
    fn counts(v: &[CoalesceClass]) -> CoalesceCounts {
        coalescing_counts(v.iter().copied())
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(counts(&[]), CoalesceCounts::default());
    }

    #[test]
    fn two_clean_same_dst_composites_fold() {
        let c = counts(&[comp(1), comp(1)]);
        assert_eq!(c.pass_ops, 2);
        assert_eq!(c.coalescable, 1);
        assert_eq!(c.mergeable, 1);
        assert_eq!(c.self_sample, 0);
    }

    #[test]
    fn different_dst_does_not_fold() {
        let c = counts(&[comp(1), comp(2)]);
        assert_eq!(c.pass_ops, 2);
        assert_eq!(c.coalescable, 0);
        assert_eq!(c.mergeable, 0);
    }

    #[test]
    fn clear_follower_is_dirty_clear_bucket() {
        // 2nd composite (consecutive same-dst) blocked only by a clear:
        // not mergeable today, but the Slice-1.5 dirty_clear bucket.
        let c = counts(&[comp(1), comp_clear(1)]);
        assert_eq!(c.coalescable, 1);
        assert_eq!(c.mergeable, 0);
        assert_eq!(c.coalescable_dirty_clear, 1);
        assert_eq!(c.coalescable_cross_kind, 0);
    }

    #[test]
    fn clear_op_opens_session_for_a_clean_follower() {
        // clear-op opens a session; the next clean same-dst composite folds.
        let c = counts(&[comp_clear(1), comp(1)]);
        assert_eq!(c.coalescable, 1);
        assert_eq!(c.mergeable, 1);
        assert_eq!(c.coalescable_dirty_clear, 0);
    }

    #[test]
    fn readback_follower_is_cross_kind_not_dirty_clear() {
        // A dst-readback composite reads dst → not unlockable by a
        // solid scratch; it belongs to the cross-kind bucket.
        let c = counts(&[comp(1), comp_readback(1)]);
        assert_eq!(c.coalescable, 1);
        assert_eq!(c.coalescable_dirty_clear, 0);
        assert_eq!(c.coalescable_cross_kind, 1);
    }

    #[test]
    fn intervening_glyph_breaks_composite_session_only() {
        // glyph is same-dst → still coalescable (all-kinds), but it
        // breaks the composite-only run so neither composite folds.
        // glyph repeat + the trailing composite are both cross-kind.
        let c = counts(&[comp(1), glyph(1), comp(1)]);
        assert_eq!(c.pass_ops, 3);
        assert_eq!(c.coalescable, 2);
        assert_eq!(c.mergeable, 0);
        assert_eq!(c.coalescable_cross_kind, 2);
        assert_eq!(c.coalescable_dirty_clear, 0);
    }

    #[test]
    fn clear_after_glyph_is_cross_kind_not_dirty_clear() {
        // A clear-blocked composite whose same-dst predecessor is a
        // glyph cannot be unlocked by a solid scratch alone (the glyph
        // still splits the run) — cross-kind, not dirty_clear.
        let c = counts(&[glyph(1), comp_clear(1)]);
        assert_eq!(c.coalescable, 1);
        assert_eq!(c.coalescable_dirty_clear, 0);
        assert_eq!(c.coalescable_cross_kind, 1);
    }

    #[test]
    fn non_pass_op_resets_runs() {
        // CoalesceClass::NonPass between same-dst composites.
        let c = counts(&[comp(1), CoalesceClass::NonPass, comp(1)]);
        assert_eq!(c.pass_ops, 2);
        assert_eq!(c.coalescable, 0);
        assert_eq!(c.mergeable, 0);
    }

    #[test]
    fn self_sample_is_a_hard_boundary() {
        // self-sampling composite counts self_sample, opens nothing:
        // the following clean same-dst composite must NOT fold into it.
        let c = counts(&[comp_self(1), comp(1)]);
        assert_eq!(c.self_sample, 1);
        assert_eq!(c.coalescable, 1); // same dst as prev pass op
        assert_eq!(c.mergeable, 0);
    }

    #[test]
    fn long_clean_run_folds_all_but_first() {
        let c = counts(&[comp(1), comp(1), comp(1), comp(1)]);
        assert_eq!(c.pass_ops, 4);
        assert_eq!(c.coalescable, 3);
        assert_eq!(c.mergeable, 3); // 4 passes → 1, removes 3
    }

    #[test]
    fn buckets_partition_coalescable() {
        // The three buckets must sum to coalescable for any sequence.
        let seq = [
            comp(1),
            comp(1),          // mergeable
            comp_clear(1),    // dirty_clear
            comp(1),          // mergeable (clear-op opened a session)
            glyph(1),         // cross_kind (non-composite repeat)
            comp(1),          // cross_kind (after glyph)
            comp_readback(1), // cross_kind (reads dst)
            comp(2),          // different dst, no hit
            CoalesceClass::NonPass,
            comp(2), // run reset, no hit
        ];
        let c = counts(&seq);
        assert_eq!(
            c.mergeable + c.coalescable_dirty_clear + c.coalescable_cross_kind,
            c.coalescable,
            "buckets must partition coalescable exactly"
        );
        assert!(c.coalescable_dirty_clear >= 1);
        assert!(c.coalescable_cross_kind >= 1);
        assert!(c.mergeable >= 1);
    }
}

// ── Slice-2 phase-2 DstPassSession decision fn (pure) ──
// fill/logic are the ONLY eligible kinds this phase; everything else
// (composite incl. fold-clean, glyph, traps, masked_copy_area,
// layout_transition, NonPass) is INELIGIBLE → flush+standalone.
mod session {
    use super::super::{CoalesceClass, DrawableId, SessionStep, session_step};

    fn d(n: u64) -> DrawableId {
        DrawableId::for_tests(n)
    }
    /// Eligible fill (or logic_fill — same class) to dst `n`.
    fn fill(n: u64) -> CoalesceClass {
        CoalesceClass::PassNonComposite {
            dst: Some(d(n)),
            is_fill_or_logic: true,
        }
    }
    /// Eligible logic_fill — identical classification to `fill`, named
    /// for readability in mixed-kind sequences.
    fn logic(n: u64) -> CoalesceClass {
        fill(n)
    }
    /// Fold-clean composite to dst `n` — session-eligible (Phase 3).
    fn comp(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: false,
            folder_clean: true,
            dirty_clear_only: false,
        }
    }
    /// Solid-clear composite to dst `n` — NOT fold-clean (pre-pass clear)
    /// → INELIGIBLE.
    fn comp_clear(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: false,
            folder_clean: false,
            dirty_clear_only: true,
        }
    }
    /// Dst-readback composite to dst `n` — NOT fold-clean → INELIGIBLE.
    fn comp_readback(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: false,
            folder_clean: false,
            dirty_clear_only: false,
        }
    }
    /// Self-sampling composite to dst `n` — NOT fold-clean → INELIGIBLE.
    fn comp_self(n: u64) -> CoalesceClass {
        CoalesceClass::Composite {
            dst: d(n),
            self_samples: true,
            folder_clean: false,
            dirty_clear_only: false,
        }
    }
    /// Ineligible glyph / image_text / traps to dst `n`.
    fn glyph(n: u64) -> CoalesceClass {
        CoalesceClass::PassNonComposite {
            dst: Some(d(n)),
            is_fill_or_logic: false,
        }
    }

    #[test]
    fn first_fill_opens() {
        assert_eq!(session_step(None, &fill(1)), SessionStep::OpenNew);
    }

    #[test]
    fn same_dst_fill_continues() {
        assert_eq!(session_step(Some(d(1)), &fill(1)), SessionStep::Continue);
    }

    #[test]
    fn same_dst_logic_continues() {
        // logic_fill is the same class as fill; same-dst → Continue.
        assert_eq!(session_step(Some(d(1)), &logic(1)), SessionStep::Continue);
    }

    #[test]
    fn different_dst_fill_flushes_then_opens() {
        assert_eq!(
            session_step(Some(d(1)), &fill(2)),
            SessionStep::FlushThenOpenNew
        );
    }

    #[test]
    fn first_clean_composite_opens() {
        // Phase 3: a fold-clean composite is session-eligible.
        assert_eq!(session_step(None, &comp(1)), SessionStep::OpenNew);
    }

    #[test]
    fn same_dst_clean_composite_continues() {
        // Fold-clean composite continues a same-dst composite session.
        assert_eq!(session_step(Some(d(1)), &comp(1)), SessionStep::Continue);
    }

    #[test]
    fn clean_composite_continues_a_fill_session() {
        // Cross-kind merge: a fill opens, a same-dst fold-clean composite
        // continues the SAME session (no flush).
        assert_eq!(session_step(Some(d(1)), &comp(1)), SessionStep::Continue);
    }

    #[test]
    fn fill_continues_a_composite_session() {
        // Cross-kind merge the other way: a same-dst fill continues a
        // composite-opened session.
        assert_eq!(session_step(Some(d(1)), &fill(1)), SessionStep::Continue);
    }

    #[test]
    fn different_dst_clean_composite_flushes_then_opens() {
        assert_eq!(
            session_step(Some(d(1)), &comp(2)),
            SessionStep::FlushThenOpenNew
        );
    }

    #[test]
    fn dirty_clear_composite_is_ineligible_flushes_then_standalone() {
        // Solid-clear composite is NOT fold-clean (pre-pass clear illegal
        // mid-pass) → flush + standalone.
        assert_eq!(
            session_step(Some(d(1)), &comp_clear(1)),
            SessionStep::FlushThenStandalone
        );
    }

    #[test]
    fn readback_composite_is_ineligible_flushes_then_standalone() {
        // Dst-readback composite reads its own dst → flush + standalone.
        assert_eq!(
            session_step(Some(d(1)), &comp_readback(1)),
            SessionStep::FlushThenStandalone
        );
    }

    #[test]
    fn self_sample_composite_is_ineligible_flushes_then_standalone() {
        // Self-sampling composite (src/mask == dst) → flush + standalone.
        assert_eq!(
            session_step(Some(d(1)), &comp_self(1)),
            SessionStep::FlushThenStandalone
        );
    }

    #[test]
    fn dirty_composite_no_session_is_standalone() {
        assert_eq!(session_step(None, &comp_clear(1)), SessionStep::Standalone);
        assert_eq!(
            session_step(None, &comp_readback(1)),
            SessionStep::Standalone
        );
        assert_eq!(session_step(None, &comp_self(1)), SessionStep::Standalone);
    }

    #[test]
    fn glyph_is_ineligible_flushes_then_standalone() {
        assert_eq!(
            session_step(Some(d(1)), &glyph(1)),
            SessionStep::FlushThenStandalone
        );
    }

    #[test]
    fn masked_copy_area_flushes_then_standalone() {
        // copy / masked_copy_area / put_image / clip-snapshot are all
        // NonPass → ineligible. With an open session → flush+standalone.
        assert_eq!(
            session_step(Some(d(1)), &CoalesceClass::NonPass),
            SessionStep::FlushThenStandalone
        );
    }

    #[test]
    fn layout_transition_flushes_then_standalone() {
        // LayoutTransition classifies as NonPass (can target the open
        // dst) → hard flush before its standalone emit.
        assert_eq!(
            session_step(Some(d(1)), &CoalesceClass::NonPass),
            SessionStep::FlushThenStandalone
        );
    }

    #[test]
    fn non_pass_no_session_is_standalone() {
        assert_eq!(
            session_step(None, &CoalesceClass::NonPass),
            SessionStep::Standalone
        );
    }
}

/// Build a `PhysicalDeviceMemoryProperties` from a list of per-type
/// property-flag sets (heap indices don't matter for type selection).
fn mem_props_with(types: &[vk::MemoryPropertyFlags]) -> vk::PhysicalDeviceMemoryProperties {
    let mut mp = vk::PhysicalDeviceMemoryProperties {
        memory_type_count: types.len() as u32,
        ..Default::default()
    };
    for (i, &flags) in types.iter().enumerate() {
        mp.memory_types[i].property_flags = flags;
    }
    mp
}

#[test]
fn readback_prefers_cached_coherent_no_invalidate() {
    use vk::MemoryPropertyFlags as F;
    // DEVICE_LOCAL, write-combined coherent, then cached+coherent.
    let mp = mem_props_with(&[
        F::DEVICE_LOCAL,
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED | F::HOST_COHERENT,
    ]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, true).expect("a host type exists");
    assert_eq!(
        idx, 2,
        "must pick the cached+coherent type, not write-combined"
    );
    assert!(coherent, "cached+coherent ⇒ no manual invalidate needed");
}

#[test]
fn readback_falls_back_to_cached_noncoherent_needs_invalidate() {
    use vk::MemoryPropertyFlags as F;
    // Only write-combined coherent + cached-non-coherent on offer.
    let mp = mem_props_with(&[
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED,
    ]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, true).expect("a host type exists");
    assert_eq!(idx, 1, "cached beats write-combined for readback");
    assert!(
        !coherent,
        "cached-only ⇒ caller must invalidate before reading"
    );
}

#[test]
fn readback_falls_back_to_coherent_when_no_cached() {
    use vk::MemoryPropertyFlags as F;
    let mp = mem_props_with(&[F::DEVICE_LOCAL, F::HOST_VISIBLE | F::HOST_COHERENT]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, true).expect("a host type exists");
    assert_eq!(
        idx, 1,
        "write-combined coherent is the last-resort readback type"
    );
    assert!(coherent);
}

#[test]
fn upload_ignores_cached_and_takes_coherent() {
    use vk::MemoryPropertyFlags as F;
    // Even with a cached type present, the upload path wants COHERENT.
    let mp = mem_props_with(&[
        F::HOST_VISIBLE | F::HOST_CACHED,
        F::HOST_VISIBLE | F::HOST_COHERENT,
    ]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, false).expect("a coherent type exists");
    assert_eq!(
        idx, 1,
        "upload selects HOST_COHERENT regardless of cached availability"
    );
    assert!(coherent);
}

#[test]
fn pick_memory_type_respects_type_bits_mask() {
    use vk::MemoryPropertyFlags as F;
    // The ideal cached+coherent type (index 1) is masked out by type_bits,
    // so readback must fall through to the write-combined coherent (index 0).
    let mp = mem_props_with(&[
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED | F::HOST_COHERENT,
    ]);
    let bits = 0b01; // only type 0 allowed
    let (idx, _) = StagingBuffer::pick_memory_type(&mp, bits, true).expect("masked selection");
    assert_eq!(
        idx, 0,
        "must honour memory_type_bits even when a better type exists"
    );
}

#[test]
fn pick_memory_type_none_when_no_host_visible() {
    use vk::MemoryPropertyFlags as F;
    let mp = mem_props_with(&[F::DEVICE_LOCAL]);
    assert!(StagingBuffer::pick_memory_type(&mp, u32::MAX, true).is_none());
    assert!(StagingBuffer::pick_memory_type(&mp, u32::MAX, false).is_none());
}

#[test]
fn close_open_frame_with_no_open_frame_returns_already_closed() {
    let mut engine = RenderEngine::stub();
    let mut store = DrawableStore::stub();
    let mut platform = PlatformBackend::for_tests();
    let out = engine
        .close_open_frame(
            &mut store,
            &mut platform,
            super::super::frame_builder::CloseReason::Shutdown,
        )
        .expect("close on a closed frame must Ok");
    assert!(matches!(
        out,
        super::super::frame_builder::CloseOutcome::AlreadyClosed
    ));
}

#[test]
fn stub_engine_declines_paint_ops() {
    let mut engine = RenderEngine::stub();
    let mut store = DrawableStore::new();
    let mut platform = PlatformBackend::for_tests();
    let storage = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();
    let err = engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            [1.0, 0.0, 0.0, 1.0],
        )
        .expect_err("stub engine must reject");
    assert!(matches!(err, RenderError::NoVk));
    assert!(!engine.is_live());
}

#[test]
fn decode_pixel_bgra_round_trip() {
    // 0xAARRGGBB → r,g,b,a in 0..1
    let rgba = decode_x11_pixel_bgra(0xFF_80_40_20);
    assert!((rgba[0] - 128.0 / 255.0).abs() < 1e-3); // R = 0x80
    assert!((rgba[1] - 64.0 / 255.0).abs() < 1e-3); // G = 0x40
    assert!((rgba[2] - 32.0 / 255.0).abs() < 1e-3); // B = 0x20
    assert!((rgba[3] - 255.0 / 255.0).abs() < 1e-3); // A = 0xFF
}

#[test]
fn x11_row_stride_pad_to_32_bits() {
    // depth-1, width 9 → 9 bits → ceil(9/32)*4 = 4 bytes.
    assert_eq!(x11_src_row_stride(1, 9), 4);
    // depth-1, width 33 → ceil(33/32)*4 = 8.
    assert_eq!(x11_src_row_stride(1, 33), 8);
    // depth-4 is nibble-packed and padded to 32 bits.
    assert_eq!(x11_src_row_stride(4, 3), 4);
    assert_eq!(x11_src_row_stride(4, 9), 8);
    // depth-8, width 3 → 24 bits padded to 32 → 4 bytes.
    assert_eq!(x11_src_row_stride(8, 3), 4);
    // depth-8, width 5 → 40 bits padded to 64 → 8 bytes.
    assert_eq!(x11_src_row_stride(8, 5), 8);
    // depth-32, width 10 → 320 bits = 40 bytes (already aligned).
    assert_eq!(x11_src_row_stride(32, 10), 40);
}

// ───── #133 step 3 (P4) — the bounds-aware clamp/scissor layer ─────
//
// Every op's clip site moved from "the storage extent" to "the
// destination handle's content bounds". These four tests pin the two
// properties the whole step rests on: at a full-extent bounds the
// new helpers are the OLD helpers (so `bw == 0` is a pure refactor),
// and at a narrower bounds they confine the op to the content.

fn brd_extent(w: u32, h: u32) -> vk::Extent2D {
    vk::Extent2D {
        width: w,
        height: h,
    }
}

fn brd_rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: brd_extent(w, h),
    }
}

#[test]
fn clamp_rect_to_full_extent_equals_clamp_rect() {
    let ext = brd_extent(24, 16);
    for r in [
        brd_rect(-8, -8, 64, 64),
        brd_rect(0, 0, 24, 16),
        brd_rect(20, 12, 8, 8),
        brd_rect(30, 30, 4, 4),
    ] {
        assert_eq!(
            clamp_rect_to(r, brd_rect(0, 0, ext.width, ext.height)),
            clamp_rect(r, ext),
            "full-extent bounds must reproduce clamp_rect for {r:?}",
        );
    }
    // Narrower bounds (content at (4, 4) inside 24x16 storage).
    assert_eq!(
        clamp_rect_to(brd_rect(-8, -8, 64, 64), brd_rect(4, 4, 16, 8)),
        brd_rect(4, 4, 16, 8),
    );
}

#[test]
fn clamp_put_rect_to_crops_the_source_at_the_content_origin() {
    let src = brd_extent(24, 16);
    // Full-extent bounds == the legacy helper.
    assert_eq!(
        clamp_put_rect_to(vk::Offset2D { x: -4, y: -4 }, src, brd_rect(0, 0, 24, 16)),
        clamp_put_rect(vk::Offset2D { x: -4, y: -4 }, src, brd_extent(24, 16)),
    );
    // Content bounds (4, 4, 16, 8): a PutImage at content-local
    // (-4, -4) — storage (0, 0) — is cropped by 4 rows/columns and
    // lands at the content origin, not in the ring.
    let (rect, (sx, sy)) =
        clamp_put_rect_to(vk::Offset2D { x: 0, y: 0 }, src, brd_rect(4, 4, 16, 8))
            .expect("visible");
    assert_eq!(rect, brd_rect(4, 4, 16, 8));
    assert_eq!((sx, sy), (4, 4), "leading source rows/cols cropped");
}

#[test]
fn clamp_copy_rects_to_clips_both_sides_and_keeps_them_aligned() {
    let ext = brd_extent(24, 16);
    // Full-extent bounds on both sides == the legacy helper.
    assert_eq!(
        clamp_copy_rects_to(
            brd_rect(-2, 0, 8, 8),
            vk::Offset2D { x: 0, y: 0 },
            brd_rect(0, 0, 24, 16),
            brd_rect(0, 0, 24, 16),
        ),
        clamp_copy_rects(brd_rect(-2, 0, 8, 8), vk::Offset2D { x: 0, y: 0 }, ext, ext),
    );
    // SOURCE bounds = content (4, 4, 16, 8): a read starting in the
    // ring advances BOTH origins, so no ring pixel is copied and the
    // destination stays aligned with the source.
    let (s, d) = clamp_copy_rects_to(
        brd_rect(0, 0, 24, 16),
        vk::Offset2D { x: 0, y: 0 },
        brd_rect(4, 4, 16, 8),
        brd_rect(0, 0, 24, 16),
    )
    .expect("visible");
    assert_eq!(s, brd_rect(4, 4, 16, 8));
    assert_eq!(d, brd_rect(4, 4, 16, 8));
}

#[test]
fn build_render_clip_scissors_to_bounds_the_no_clip_case() {
    let ext = brd_extent(24, 16);
    // No picture clip + full-extent bounds == the legacy helper.
    assert_eq!(
        build_render_clip_scissors_to(None, brd_rect(0, 0, 24, 16)),
        build_render_clip_scissors(None, ext),
    );
    // No picture clip + content bounds → the content rect itself.
    assert_eq!(
        build_render_clip_scissors_to(None, brd_rect(4, 4, 16, 8)),
        vec![brd_rect(4, 4, 16, 8)],
    );
    // A client clip that reaches into the ring is trimmed to it.
    let cr = [Rectangle16 {
        x: -8,
        y: -8,
        width: 64,
        height: 64,
    }];
    assert_eq!(
        build_render_clip_scissors_to(Some(&cr), brd_rect(4, 4, 16, 8)),
        vec![brd_rect(4, 4, 16, 8)],
    );
}

#[test]
fn clamp_put_rect_inside_returns_unchanged() {
    let r = clamp_put_rect(
        vk::Offset2D { x: 2, y: 3 },
        vk::Extent2D {
            width: 4,
            height: 5,
        },
        vk::Extent2D {
            width: 16,
            height: 16,
        },
    )
    .unwrap();
    assert_eq!(r.0.offset, vk::Offset2D { x: 2, y: 3 });
    assert_eq!(
        r.0.extent,
        vk::Extent2D {
            width: 4,
            height: 5,
        },
    );
    assert_eq!(r.1, (0, 0));
}

#[test]
fn clamp_put_rect_partial_clip_records_source_offset() {
    // dst_pos = (-1, -2), src 4×5 against a 16×16 storage →
    // dst rect (0,0,3,3) with source-input origin (1, 2).
    let r = clamp_put_rect(
        vk::Offset2D { x: -1, y: -2 },
        vk::Extent2D {
            width: 4,
            height: 5,
        },
        vk::Extent2D {
            width: 16,
            height: 16,
        },
    )
    .unwrap();
    assert_eq!(r.0.offset, vk::Offset2D { x: 0, y: 0 });
    assert_eq!(
        r.0.extent,
        vk::Extent2D {
            width: 3,
            height: 3,
        },
    );
    assert_eq!(r.1, (1, 2));
}

#[test]
fn clamp_put_rect_outside_returns_none() {
    let r = clamp_put_rect(
        vk::Offset2D { x: 100, y: 100 },
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Extent2D {
            width: 16,
            height: 16,
        },
    );
    assert!(r.is_none());
}

#[test]
fn depth1_unpack_round_trip() {
    // 1×8 source padded to a 32-bit scanline (4 bytes). Bit
    // order LSB-first per the server's advertised
    // `bitmap-bit-order`: 0xAA = 1010_1010 = bits 1, 3, 5, 7
    // set → pixels 1, 3, 5, 7 set. Remaining 3 bytes are
    // scanline pad.
    let src = vec![0xAAu8, 0x00, 0x00, 0x00];
    let src_extent = vk::Extent2D {
        width: 8,
        height: 1,
    };
    let mut out = vec![0u8; 8];
    unpack_to_staging(&src, src_extent, 0, 0, 8, 1, 1, out.as_mut_ptr()).unwrap();
    assert_eq!(out, vec![0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]);

    let packed = pack_from_storage(&out, 8, 1, 1).unwrap();
    // Row stride is 4 bytes (32 bits) per depth-1 pad rule;
    // the first byte holds the data, repacked LSB-first →
    // 0xAA round-trips (the byte is self-symmetric under
    // pack/unpack inversion).
    assert_eq!(packed.len(), 4);
    assert_eq!(packed[0], 0xAA);
}

#[test]
fn depth32_unpack_is_memcpy() {
    // 2×2 BGRA8 source.
    let src: Vec<u8> = vec![
        0x10, 0x20, 0x30, 0xFF, 0x11, 0x21, 0x31, 0xFF, // row 0
        0x12, 0x22, 0x32, 0xFF, 0x13, 0x23, 0x33, 0xFF, // row 1
    ];
    let src_extent = vk::Extent2D {
        width: 2,
        height: 2,
    };
    let mut out = vec![0u8; 16];
    unpack_to_staging(&src, src_extent, 0, 0, 2, 2, 32, out.as_mut_ptr()).unwrap();
    assert_eq!(out, src);
}

#[test]
fn depth4_unpack_and_pack_follow_nibble_layout() {
    let src = vec![0x21u8, 0x00, 0x00, 0x00];
    let src_extent = vk::Extent2D {
        width: 2,
        height: 1,
    };
    let mut out = vec![0u8; 2];
    unpack_to_staging(&src, src_extent, 0, 0, 2, 1, 4, out.as_mut_ptr()).unwrap();
    assert_eq!(out, vec![0x01, 0x02]);

    let packed = pack_from_storage(&out, 2, 1, 4).unwrap();
    assert_eq!(packed, vec![0x21, 0x00, 0x00, 0x00]);
}

// ── Vk-backed integration tests ─────────────────────────────
//
// Each `#[ignore]` test needs a live Vulkan ICD (lavapipe is
// fine). Run with:
//   `cargo test -p yserver --lib kms::render::engine::tests:: -- --ignored`
// The Stage 2 acceptance harness (Stage 2f) folds these into
// the synthetic acceptance binary.

fn live_platform() -> Option<PlatformBackend> {
    // Can't reuse `PlatformBackend::open_with_commit` here —
    // it tries to acquire a real DRM device. Tests need a
    // VkContext-only fixture. We build one by hand:
    // construct a `for_tests` fixture, then swap in a real
    // VkContext + OpsCommandPool + FencePool.
    let mut p = PlatformBackend::for_tests();
    let vk = match VkContext::new() {
        Ok(v) => v,
        Err(_) => return None,
    };
    let ops_pool = match crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)) {
        Ok(o) => o,
        Err(_) => return None,
    };
    let fence_pool = super::super::platform::FencePool::new(Arc::clone(&vk));
    p.vk = Some(vk);
    p.ops_command_pool = Some(ops_pool);
    p.fence_pool = Some(fence_pool);
    Some(p)
}

/// Alias of `live_platform` used by Task 3 tests.
fn try_for_tests_with_vk() -> Option<PlatformBackend> {
    live_platform()
}

/// Allocate a pixmap drawable in `store` backed by a real Vk
/// storage. Returns the `DrawableId`. Used by Task 3 tests.
fn create_pixmap(
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    xid: u32,
    w: u16,
    h: u16,
    depth: u8,
) -> Result<DrawableId, RenderError> {
    let storage = platform
        .allocate_drawable_storage(w, h, depth)
        .map_err(RenderError::Vk)?;
    store
        .allocate(
            xid,
            super::super::store::DrawableKind::Pixmap,
            depth,
            false,
            storage,
        )
        .map_err(|_| RenderError::NoVk)
}

/// Task 4 test helper: drive N `render_composite` (OP_OVER,
/// `src` → `dst`, no mask) calls, one per `(x_off, y_off, w, h)`
/// tuple. All calls share the same dst+src so the render-batch
/// coalescer can aggregate them into a single CB.
///
/// Panics if any call returns an error.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn drive_render_composite_same_key_for_tests(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    dst: DrawableId,
    src: DrawableId,
    rects: &[(i32, i32, u32, u32)],
) {
    const OP_OVER: u8 = 3;
    for &(x_off, y_off, w, h) in rects {
        let composite_rect = [crate::kms::vk::ops::render::CompositeRect {
            src_x: x_off,
            src_y: y_off,
            mask_x: 0,
            mask_y: 0,
            dst_x: x_off,
            dst_y: y_off,
            width: w,
            height: h,
        }];
        engine
            .render_composite(
                store,
                platform,
                OP_OVER,
                ResolvedSource::Drawable(SourceDrawable::whole(src)),
                ResolvedSource::None,
                Dst::server_internal(dst),
                &composite_rect,
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
            .expect("render_composite");
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn depth32_put_image_get_image_round_trip() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(8, 8, 32)
        .expect("alloc storage");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("store.allocate");

    // 8x8 BGRA8 gradient.
    let mut src = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let off = (y * 8 + x) * 4;
            src[off] = (x * 32) as u8; // B
            src[off + 1] = (y * 32) as u8; // G
            src[off + 2] = ((x + y) * 16) as u8; // R
            src[off + 3] = 0xFF; // A
        }
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D { x: 0, y: 0 },
            vk::Extent2D {
                width: 8,
                height: 8,
            },
            &src,
            32,
        )
        .expect("put_image");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: 8,
                    height: 8,
                },
            },
            32,
        )
        .expect("get_image");
    assert_eq!(out, src, "depth-32 round-trip must be byte-identical");

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_then_get_image_observes_clear_color() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // Fill the whole pixmap with bright red (R=0xFF, G=0, B=0, A=0xFF).
    let color = decode_x11_pixel_bgra(0xFF_FF_00_00);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            color,
        )
        .expect("fill_rect");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // Storage is BGRA8: every pixel should be [B=0, G=0, R=0xFF, A=0xFF].
    for px in out.chunks_exact(4) {
        assert_eq!(px[0], 0x00, "B");
        assert_eq!(px[1], 0x00, "G");
        assert_eq!(px[2], 0xFF, "R");
        assert_eq!(px[3], 0xFF, "A");
    }

    engine.drain_all(&mut platform);
}

/// `fill_rect` must write the source byte into `R8_UNORM`
/// storage, not treat it like BGRA. This locks the depth-8
/// GXcopy path that Xlib9 `XFillRectangle` exercises.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_depth8_observes_r8_source_byte() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(4, 4, 8).expect("alloc");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .unwrap();

    let color = decode_x11_pixel_for_storage(0x01, 8, vk::Format::R8_UNORM);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            color,
        )
        .expect("fill_rect");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            8,
        )
        .expect("get_image");
    for b in out {
        assert_eq!(b, 0x01, "R8 fill must preserve the source byte");
    }

    engine.drain_all(&mut platform);
}

/// Stage 3f.2: `engine.logic_fill` applies the per-`GcFunction`
/// `VkLogicOp` per pixel. Drives `Xor` against a pre-loaded BGRA8
/// pattern; expects each component to be the pre-load XOR'd with
/// the fg byte. Alpha is preserved via the `opaque_alpha=true`
/// pipeline (L1 server-α invariant on depth-24).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_xor_applies_per_pixel() {
    use yserver_core::backend::GcFunction;

    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // 4x4 BGRA8 pixmap. Store BGRA wire bytes B, G, R, A.
    let storage = platform.allocate_drawable_storage(4, 4, 24).expect("alloc");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            24,
            false,
            storage,
        )
        .unwrap();

    // Load every pixel with [B=0x20, G=0x40, R=0x80, A=0xFF].
    let mut pre = vec![0u8; 4 * 4 * 4];
    for px in pre.chunks_exact_mut(4) {
        px[0] = 0x20;
        px[1] = 0x40;
        px[2] = 0x80;
        px[3] = 0xFF;
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            &pre,
            32,
        )
        .expect("put_image");

    // XOR with fg pixel 0x00FFFFFF (X11 wire = AARRGGBB: A=0,
    // R=0xFF, G=0xFF, B=0xFF). The recorder's `BGRA8_UNORM`
    // branch puts R/G/B into [0]/[1]/[2] of `fg_color`; the
    // logic-op output then targets the BGRA8 attachment in the
    // same channel order, so post-XOR every component reads as
    // `pre ^ 0xFF`.
    let rect = Rectangle16 {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };
    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Xor,
            /* opaque_alpha */ true,
            /* fg */ 0x00FF_FFFF,
            &[rect],
        )
        .expect("logic_fill");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");

    for px in out.chunks_exact(4) {
        assert_eq!(px[0], 0x20 ^ 0xFF, "B (XOR pre 0x20 with fg 0xFF)");
        assert_eq!(px[1], 0x40 ^ 0xFF, "G (XOR pre 0x40 with fg 0xFF)");
        assert_eq!(px[2], 0x80 ^ 0xFF, "R (XOR pre 0x80 with fg 0xFF)");
        // opaque_alpha=true: alpha channel mask drops alpha from
        // the LogicOp, so the destination's pre-load 0xFF is
        // preserved.
        assert_eq!(px[3], 0xFF, "A preserved by opaque_alpha mask");
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_disjoint_pixmaps_round_trip() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage_src = platform.allocate_drawable_storage(4, 4, 32).unwrap();
    let storage_dst = platform.allocate_drawable_storage(8, 4, 32).unwrap();
    let src = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage_src,
        )
        .unwrap();
    let dst = store
        .allocate(
            0x2,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage_dst,
        )
        .unwrap();

    // Fill src with red.
    let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(src),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            red,
        )
        .unwrap();
    // Fill dst with blue.
    let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            blue,
        )
        .unwrap();
    // Copy src into dst at (4, 0).
    engine
        .copy_area(
            &mut store,
            &mut platform,
            Src::server_internal(src),
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            vk::Offset2D { x: 4, y: 0 },
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            32,
        )
        .unwrap();
    // Left half (0..4) should be blue (B=0xFF, G=0, R=0, A=0xFF).
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 8 + x) * 4;
            assert_eq!(&out[off..off + 4], &[0xFF, 0x00, 0x00, 0xFF], "left blue");
        }
    }
    // Right half (4..8) should be red (B=0, G=0, R=0xFF, A=0xFF).
    for y in 0..4 {
        for x in 4..8 {
            let off = (y * 8 + x) * 4;
            assert_eq!(&out[off..off + 4], &[0x00, 0x00, 0xFF, 0xFF], "right red");
        }
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_depth32_preserves_wire_alpha_when_not_opaque() {
    use yserver_core::backend::GcFunction;

    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(2, 2, 32)
        .expect("storage");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("alloc");

    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            decode_x11_pixel_bgra(0),
        )
        .expect("clear");

    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Copy,
            /* opaque_alpha */ false,
            /* fg */ 0x0000_0001,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }],
        )
        .expect("logic_fill");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            32,
        )
        .expect("get_image");

    for px in out.chunks_exact(4) {
        assert_eq!(px, &[0x01, 0x00, 0x00, 0x00]);
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_r8_not_family_matches_x11_bytes() {
    use yserver_core::backend::GcFunction;

    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(2, 1, 8)
        .expect("storage");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .expect("alloc");

    // Preload dst bytes [0x00, 0x03].
    let pre = vec![0x00, 0x03, 0x00, 0x00];
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 2,
                height: 1,
            },
            &pre,
            8,
        )
        .expect("put_image");

    let rect = Rectangle16 {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };

    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Set,
            /* opaque_alpha */ true,
            /* fg */ 0,
            &[rect],
        )
        .expect("logic_fill set");
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 1,
                },
            },
            8,
        )
        .expect("get_image set");
    assert_eq!(&out[..2], &[0xff, 0xff], "GXset must write all 1 bits");

    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 2,
                height: 1,
            },
            &pre,
            8,
        )
        .expect("put_image reload");
    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Invert,
            /* opaque_alpha */ true,
            /* fg */ 0,
            &[rect],
        )
        .expect("logic_fill invert");
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 1,
                },
            },
            8,
        )
        .expect("get_image invert");
    assert_eq!(&out[..2], &[0xff, 0xfc], "GXinvert must flip all 8 bits");

    engine.drain_all(&mut platform);
}

// GPU-level regression for the MATE compositor slow-drag-left shadow
// smear (commit fixing clamp_copy_rects). Reproduces the exact
// Present→COW shape: src_rect.offset == dst_pos == a NEGATIVE origin
// (the compositor's off-top-left damage sliver). The 2 off-screen
// columns are skipped on BOTH sides, so an 8-wide red source copied
// at x=-2 must paint dst columns 0..6 red and leave 6..8 blue. The
// old double-subtract copied only 4 columns (0..4), leaving cols 4..5
// stale blue — the trailing smear strip. Runs the real engine copy +
// GPU readback, not just the clamp arithmetic.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_negative_offset_copies_trailing_strip() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage_src = platform.allocate_drawable_storage(8, 4, 32).unwrap();
    let storage_dst = platform.allocate_drawable_storage(8, 4, 32).unwrap();
    let src = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage_src,
        )
        .unwrap();
    let dst = store
        .allocate(
            0x2,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage_dst,
        )
        .unwrap();

    let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
    let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
    let full8x4 = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 8,
            height: 4,
        },
    };
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(src),
            full8x4,
            red,
        )
        .unwrap();
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(dst),
            full8x4,
            blue,
        )
        .unwrap();

    // Aligned negative origin: src sub-rect AND dst placement both at
    // x=-2 (mirrors PresentPixmap update rect with x0<0).
    engine
        .copy_area(
            &mut store,
            &mut platform,
            Src::server_internal(src),
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D { x: -2, y: 0 },
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            vk::Offset2D { x: -2, y: 0 },
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            full8x4,
            32,
        )
        .unwrap();
    for y in 0..4 {
        for x in 0..8 {
            let off = (y * 8 + x) * 4;
            let px = &out[off..off + 4];
            if x < 6 {
                // The trailing strip cols 4..6 is what the bug dropped.
                assert_eq!(
                    px,
                    &[0x00, 0x00, 0xFF, 0xFF],
                    "col {x} must be red (copied)"
                );
            } else {
                assert_eq!(
                    px,
                    &[0xFF, 0x00, 0x00, 0xFF],
                    "col {x} must stay blue (off-copy)"
                );
            }
        }
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_self_overlap_scratch_path() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(8, 1, 32).unwrap();
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // PutImage a horizontal gradient: 8 pixels each with a
    // distinct red value.
    let mut src = vec![0u8; 8 * 4];
    for x in 0..8 {
        let off = x * 4;
        src[off] = 0x00; // B
        src[off + 1] = 0x00; // G
        src[off + 2] = (x as u8) * 0x20; // R
        src[off + 3] = 0xFF; // A
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 8,
                height: 1,
            },
            &src,
            32,
        )
        .unwrap();
    // Copy (0..4) → (2..6) (overlap; scratch path engages).
    engine
        .copy_area(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 1,
                },
            },
            vk::Offset2D { x: 2, y: 0 },
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 1,
                },
            },
            32,
        )
        .unwrap();
    // Expected R-channel sequence: [0, 0x20, 0, 0x20, 0x40, 0x60, 0xC0, 0xE0]
    // After copy of (0..4) → (2..6):
    //   col 0: original (R=0)
    //   col 1: original (R=0x20)
    //   col 2: src col 0 (R=0)
    //   col 3: src col 1 (R=0x20)
    //   col 4: src col 2 (R=0x40)
    //   col 5: src col 3 (R=0x60)
    //   col 6: original col 6 (R=0xC0)
    //   col 7: original col 7 (R=0xE0)
    let expected_r = [0x00, 0x20, 0x00, 0x20, 0x40, 0x60, 0xC0, 0xE0];
    for (x, &exp) in expected_r.iter().enumerate() {
        let off = x * 4 + 2;
        assert_eq!(
            out[off], exp,
            "R at col {x} (got {:#x}, want {exp:#x})",
            out[off]
        );
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn put_image_then_fill_overwrites() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // PutImage all-blue, then fill (1,1)..(3,3) with green.
    // B=0xFF, G=0, R=0, A=0xFF
    let blue = [0xFFu8, 0x00, 0x00, 0xFF].repeat(16);
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            &blue,
            32,
        )
        .unwrap();
    let green = decode_x11_pixel_bgra(0xFF_00_FF_00);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D { x: 1, y: 1 },
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            green,
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .unwrap();
    // (0,0) still blue.
    assert_eq!(&out[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
    // (1,1) green: B=0, G=0xFF, R=0, A=0xFF.
    let off_1_1 = (4 + 1) * 4;
    assert_eq!(&out[off_1_1..off_1_1 + 4], &[0x00, 0xFF, 0x00, 0xFF]);
    // (3,3) still blue.
    let off_3_3 = (3 * 4 + 3) * 4;
    assert_eq!(&out[off_3_3..off_3_3 + 4], &[0xFF, 0x00, 0x00, 0xFF]);

    engine.drain_all(&mut platform);
}

#[test]
fn depth24_unpack_forces_alpha_ff() {
    // Source 1×1 with X-byte (alpha-slot) = 0x55.
    let src = vec![0x10u8, 0x20, 0x30, 0x55];
    let src_extent = vk::Extent2D {
        width: 1,
        height: 1,
    };
    let mut out = vec![0u8; 4];
    unpack_to_staging(&src, src_extent, 0, 0, 1, 1, 24, out.as_mut_ptr()).unwrap();
    assert_eq!(out, vec![0x10, 0x20, 0x30, 0xFF]);
}

// ── Stage 3a Vk-backed integration tests ────────────────────

/// Helper: allocate a depth-32 storage and return a registered
/// DrawableId. Mirrors the pattern Stage 2c tests use.
fn alloc_drawable_3a(
    platform: &PlatformBackend,
    store: &mut DrawableStore,
    xid: u32,
    w: u16,
    h: u16,
) -> DrawableId {
    alloc_drawable_3a_with_kind(
        platform,
        store,
        xid,
        w,
        h,
        super::super::store::DrawableKind::Pixmap,
        false,
    )
}

fn alloc_drawable_3a_with_kind(
    platform: &PlatformBackend,
    store: &mut DrawableStore,
    xid: u32,
    w: u16,
    h: u16,
    kind: super::super::store::DrawableKind,
    scene_participating: bool,
) -> DrawableId {
    let storage = platform
        .allocate_drawable_storage(w, h, 32)
        .expect("alloc storage");
    store
        .allocate(xid, kind, 32, scene_participating, storage)
        .expect("store allocate")
}

/// Build a `PreparedGlyph` with `w × h` filled bytes (the
/// fill byte is 0xFF so the shader paints solid foreground).
fn build_glyph(codepoint: u32, dst_x: i32, dst_y: i32, w: usize, h: usize) -> PreparedGlyph {
    PreparedGlyph {
        dst_x,
        dst_y,
        w,
        h,
        pixels: vec![0xFF_u8; w * h],
        codepoint,
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn image_text_run_records_damage_on_target() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Window-kind + scene-participating so presentation damage
    // accumulates (per the I5 spec amendment, pixmaps no longer
    // accumulate any damage in the store — protocol DamageNotify
    // fanout lives at the request layer).
    let id = alloc_drawable_3a_with_kind(
        &platform,
        &mut store,
        0x1,
        64,
        32,
        super::super::store::DrawableKind::Window,
        true,
    );
    // Two glyphs spanning x=[10..22] × y=[5..17].
    let glyphs = vec![
        build_glyph(u32::from(b'A'), 10, 5, 6, 12),
        build_glyph(u32::from(b'B'), 16, 5, 6, 12),
    ];
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            7,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
        )
        .expect("image_text");
    assert_eq!(stats.atlas_interns, 2);
    assert_eq!(stats.glyph_uploads, 2);
    assert_eq!(stats.glyphs_dropped, 0);

    // Damage union covers the two glyph quads.
    let d = store.get(id).expect("drawable");
    let rects: Vec<vk::Rect2D> = d.presentation_damage.rects().to_vec();
    assert!(!rects.is_empty(), "presentation damage should be set");
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for r in rects {
        min_x = min_x.min(r.offset.x);
        min_y = min_y.min(r.offset.y);
        max_x = max_x.max(r.offset.x + r.extent.width as i32);
        max_y = max_y.max(r.offset.y + r.extent.height as i32);
    }
    assert!(min_x <= 10);
    assert!(min_y <= 5);
    assert!(max_x >= 22);
    assert!(max_y >= 17);

    engine.drain_all(&mut platform);
}

/// **Load-bearing per codex round 1**: two back-to-back glyph
/// uploads with distinct keys must not corrupt each other's
/// atlas pixels. v1's shared persistent staging would clobber
/// A when B's memcpy lands while A's GPU read is in flight; the
/// v2 per-upload arena slice rules that out.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn atlas_back_to_back_upload_no_corruption() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

    // Pre-clear the target to black.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 32,
                    height: 32,
                },
            },
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

    // Two glyphs with distinguishable solid-alpha rectangles.
    // The text shader does `foreground × atlas.r`; with
    // 0xFF-filled atlas and white foreground, the dst quads
    // come out (B=0xFF, G=0xFF, R=0xFF, A=0xFF).
    let glyphs = vec![
        build_glyph(u32::from(b'A'), 1, 1, 4, 4),
        build_glyph(u32::from(b'B'), 10, 1, 4, 4),
    ];
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            42,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
        )
        .expect("image_text");
    assert_eq!(stats.atlas_interns, 2);

    // Read back: both quads should be white; pixels between
    // them should be the original black.
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 32,
                    height: 32,
                },
            },
            32,
        )
        .expect("get_image");
    let pixel_at = |x: usize, y: usize| {
        let off = (y * 32 + x) * 4;
        (out[off], out[off + 1], out[off + 2], out[off + 3])
    };
    // A's quad: (1..5, 1..5).
    for y in 1..5 {
        for x in 1..5 {
            let (b, g, r, _a) = pixel_at(x, y);
            assert_eq!(
                (b, g, r),
                (0xFF, 0xFF, 0xFF),
                "glyph A quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
            );
        }
    }
    // B's quad: (10..14, 1..5).
    for y in 1..5 {
        for x in 10..14 {
            let (b, g, r, _a) = pixel_at(x, y);
            assert_eq!(
                (b, g, r),
                (0xFF, 0xFF, 0xFF),
                "glyph B quad pixel ({x},{y}) corrupted: ({b:#x},{g:#x},{r:#x})",
            );
        }
    }
    // Between the quads (7, 2) should still be black.
    let (b, g, r, _a) = pixel_at(7, 2);
    assert_eq!(
        (b, g, r),
        (0x00, 0x00, 0x00),
        "between-quad pixel (7,2) should be background black; got ({b:#x},{g:#x},{r:#x})"
    );

    engine.drain_all(&mut platform);
}

fn atlas_resets(engine: &RenderEngine) -> u64 {
    engine
        .inner
        .as_ref()
        .and_then(|i| i.glyph_atlas.as_ref())
        .map_or(0, GlyphAtlas::resets)
}

fn atlas_has(engine: &RenderEngine, font_xid: u32, codepoint: u32) -> bool {
    engine
        .inner
        .as_ref()
        .and_then(|i| i.glyph_atlas.as_ref())
        .and_then(|a| {
            a.lookup(GlyphKey {
                font_xid,
                codepoint,
            })
        })
        .is_some()
}

/// A `w × h` glyph whose coverage is 0xFF in columns `cols` of its
/// top four rows and 0 everywhere else.
fn corner_glyph(
    codepoint: u32,
    dst_x: i32,
    dst_y: i32,
    w: usize,
    h: usize,
    cols: std::ops::Range<usize>,
) -> PreparedGlyph {
    let mut g = build_glyph(codepoint, dst_x, dst_y, w, h);
    g.pixels.fill(0);
    for y in 0..4 {
        for x in cols.clone() {
            g.pixels[y * w + x] = 0xFF;
        }
    }
    g
}

/// Two 2049² glyphs cannot share the 4096² atlas. The second draw
/// arrives while the first one's upload and draw are still only
/// recorded in the open frame: the atlas must close that frame,
/// reset, and hand the second glyph the slot the first one used —
/// and both draws must still show their own glyph.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn atlas_full_resets_behind_the_recorded_draw() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 8);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 8,
        },
    };
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

    // Covered: g0 columns 0..4 at x 1..5; g1 columns 4..8 at x 16..20.
    let g0 = corner_glyph(1, 1, 1, 2049, 2049, 0..4);
    let g1 = corner_glyph(2, 12, 1, 2049, 2049, 4..8);
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            1,
            [1.0, 1.0, 1.0, 1.0],
            &[g0],
        )
        .expect("first image_text");
    assert_eq!((stats.atlas_interns, stats.glyphs_dropped), (1, 0));
    assert!(
        engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .is_open(),
        "the first draw must still be unsubmitted for this test to mean anything",
    );

    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            1,
            [1.0, 1.0, 1.0, 1.0],
            &[g1],
        )
        .expect("second image_text");
    assert_eq!((stats.atlas_interns, stats.glyphs_dropped), (1, 0));
    assert_eq!(atlas_resets(&engine), 1);
    assert!(
        !atlas_has(&engine, 1, 1),
        "the reset dropped the first glyph"
    );
    let pending = engine
        .inner
        .as_ref()
        .and_then(|i| i.frame_builder.open.as_ref())
        .map(|o| o.pending_glyph_inserts.entries.clone())
        .expect("the second draw opened a new frame");
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].1.atlas_x, pending[0].1.atlas_y),
        (0, 0),
        "the second glyph reuses the first one's slot",
    );

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image");
    for y in 0..8 {
        for x in 0..32 {
            let want = (1..5).contains(&y) && ((1..5).contains(&x) || (16..20).contains(&x));
            let off = (y * 32 + x) * 4;
            let px = (out[off], out[off + 1], out[off + 2]);
            let expect = if want { (0xFF, 0xFF, 0xFF) } else { (0, 0, 0) };
            assert_eq!(px, expect, "pixel ({x},{y})");
        }
    }
    engine.drain_all(&mut platform);
}

/// A glyph wider than the whole atlas can never be placed: it
/// drops (rate-limited warning) and must NOT empty the atlas.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn glyph_larger_than_atlas_drops_without_reset() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 4, 4);
    let small = build_glyph(1, 0, 0, 4, 4);
    let huge = build_glyph(2, 0, 0, 4097, 1);
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            1,
            [1.0, 1.0, 1.0, 1.0],
            &[small, huge],
        )
        .expect("image_text");
    assert_eq!((stats.atlas_interns, stats.glyphs_dropped), (1, 1));
    assert_eq!(atlas_resets(&engine), 0);
    engine
        .close_open_frame(
            &mut store,
            &mut platform,
            super::super::frame_builder::CloseReason::SyncWait,
        )
        .expect("close");
    assert!(atlas_has(&engine, 1, 1));
    engine.drain_all(&mut platform);
}

/// `forget_glyphs` drops committed entries AND inserts still
/// pending in the open frame, so a redefined glyph id cannot be
/// served its old image once that frame commits.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn forget_glyphs_drops_committed_and_pending_entries() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 16, 4);
    let close =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .close_open_frame(
                    store,
                    platform,
                    super::super::frame_builder::CloseReason::SyncWait,
                )
                .expect("close");
        };
    let draw = |engine: &mut RenderEngine,
                store: &mut DrawableStore,
                platform: &mut PlatformBackend,
                font: u32| {
        let glyphs = [build_glyph(1, 0, 0, 2, 2), build_glyph(2, 4, 0, 2, 2)];
        engine
            .image_text(
                store,
                platform,
                Dst::server_internal(target),
                font,
                [1.0, 1.0, 1.0, 1.0],
                &glyphs,
            )
            .expect("image_text");
    };
    // Committed: font 10's glyphs land in the atlas, then go.
    draw(&mut engine, &mut store, &mut platform, 10);
    close(&mut engine, &mut store, &mut platform);
    assert!(atlas_has(&engine, 10, 1) && atlas_has(&engine, 10, 2));
    engine.forget_glyphs(10, Some(&[1]));
    assert!(!atlas_has(&engine, 10, 1));
    assert!(atlas_has(&engine, 10, 2), "only the named id goes");
    engine.forget_glyphs(10, None);
    assert!(!atlas_has(&engine, 10, 2));

    // Pending: font 11's inserts are still in the open frame.
    draw(&mut engine, &mut store, &mut platform, 11);
    engine.forget_glyphs(11, Some(&[2]));
    close(&mut engine, &mut store, &mut platform);
    assert!(atlas_has(&engine, 11, 1));
    assert!(
        !atlas_has(&engine, 11, 2),
        "a forgotten pending insert never commits"
    );
    engine.drain_all(&mut platform);
}

/// Step 2 proof (component-alpha glyphs plan,
/// `docs/superpowers/plans/2026-09-10-component-alpha-glyphs-plan.md`):
/// `AtlasEntry.w` split into `packed_w` (atlas footprint) and
/// `logical_w` (the glyph's own size). While the two agree, either
/// field paints identical pixels, so a green suite says nothing
/// about which one a given consumer actually reads — this test
/// manufactures a cache entry where they DISAGREE and checks each
/// consumer against the correct one, through the real
/// `image_text` code path (not a reimplementation of it).
///
/// A first glyph is uploaded for real at packed_w == logical_w ==
/// 40, with its 40 texels spatially varying — left half (cols
/// 0..20) opaque, right half (20..40) transparent — so the atlas
/// holds content a wrong-width sample would visibly disagree with.
/// Its cache entry is then overwritten in place (same atlas slot,
/// same uploaded pixels) with `logical_w` shrunk to 10 while
/// `packed_w` stays 40. A second `image_text` call at the SAME
/// glyph key is then a committed cache hit: it never re-uploads,
/// so every downstream value comes from the (now-asymmetric)
/// `AtlasEntry`, not from anything this test computes itself.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn atlas_entry_packed_vs_logical_width_feed_the_right_consumers() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let real_target = alloc_drawable_3a(&platform, &mut store, 0x1, 64, 32);
    // Window + scene-participating so presentation damage
    // accumulates (mirrors `image_text_run_records_damage_on_target`).
    let probe_target = alloc_drawable_3a_with_kind(
        &platform,
        &mut store,
        0x2,
        64,
        32,
        super::super::store::DrawableKind::Window,
        true,
    );

    // Pre-clear the probe target to black so a painted quad — or
    // the absence of one — is unambiguous on readback.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(probe_target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 64,
                    height: 32,
                },
            },
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

    // Real 40×20 upload: left half (texels 0..20) opaque, right
    // half (20..40) transparent.
    let font_xid = 4242;
    let codepoint = u32::from(b'Z');
    let (w, h) = (40usize, 20usize);
    let mut pixels = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..20 {
            pixels[row * w + col] = 0xFF;
        }
    }
    let real_glyph = PreparedGlyph {
        dst_x: 0,
        dst_y: 0,
        w,
        h,
        pixels,
        codepoint,
    };
    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(real_target),
            font_xid,
            [1.0, 1.0, 1.0, 1.0],
            &[real_glyph],
        )
        .expect("image_text (real upload)");
    assert_eq!(stats.atlas_interns, 1);
    assert_eq!(stats.glyph_uploads, 1);

    // The glyph insert is transactional — pending until the frame
    // closes (`commit_close_success`). Close it now so the entry
    // is actually in `glyph_atlas`'s cache before we read it back.
    engine
        .close_open_frame(
            &mut store,
            &mut platform,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        )
        .expect("close frame after real upload");

    // Overwrite the cached entry: same atlas slot (same uploaded
    // pixels), but logical_w now disagrees with packed_w.
    let key = GlyphKey {
        font_xid,
        codepoint,
    };
    let inner = engine.inner.as_mut().expect("inner");
    let atlas = inner
        .glyph_atlas
        .as_mut()
        .expect("atlas init by first call");
    let real_entry = atlas.lookup(key).expect("entry cached by real upload");
    assert_eq!(real_entry.packed_w, 40);
    assert_eq!(real_entry.logical_w, 40);
    let asymmetric_entry = AtlasEntry {
        packed_w: 40,
        logical_w: 10,
        ..real_entry
    };
    atlas.insert_entry(key, asymmetric_entry);

    // Second call, same key: a committed hit. dst_x/dst_y/w/h/pixels
    // on this input glyph are irrelevant on the hit path — only the
    // cached entry's fields drive geometry — so they're placeholders.
    let probe_glyph = PreparedGlyph {
        dst_x: 5,
        dst_y: 5,
        w: 1,
        h: 1,
        pixels: vec![0u8; 1],
        codepoint,
    };
    let stats2 = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(probe_target),
            font_xid,
            [1.0, 1.0, 1.0, 1.0],
            &[probe_glyph],
        )
        .expect("image_text (cache hit)");
    assert_eq!(
        stats2.atlas_interns, 0,
        "must be a cache hit, not a re-upload, or this proves nothing"
    );
    assert_eq!(stats2.glyph_uploads, 0);

    // (1) Damage extent: the append-time damage union must use
    // logical_w (10), not packed_w (40).
    let d = store.get(probe_target).expect("drawable");
    let rects: Vec<vk::Rect2D> = d.presentation_damage.rects().to_vec();
    let probe_rect = rects
        .iter()
        .find(|r| r.offset.x == 5 && r.offset.y == 5)
        .unwrap_or_else(|| panic!("no damage rect at (5,5): {rects:?}"));
    assert_eq!(
        probe_rect.extent.width, 10,
        "damage extent used packed_w (40) instead of logical_w (10)"
    );
    assert_eq!(probe_rect.extent.height, 20);

    // (2) Instance geometry: the dst quad is logical_w (10) wide,
    // and its atlas UV span must ALSO be logical_w wide (never the
    // packed footprint) — so it samples only texels 0..10, a
    // subset of the real upload's opaque 0..20, and paints fully
    // opaque white. Had the instance geometry used packed_w (40)
    // for the atlas span instead, the 10-pixel-wide quad would
    // stretch across all 40 texels and its right half would land
    // on the transparent texels 20..40, producing a visibly mixed
    // opaque/transparent pattern instead of a solid one.
    engine.drain_all(&mut platform);
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(probe_target),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 64,
                    height: 32,
                },
            },
            32,
        )
        .expect("get_image");
    let pixel_at = |x: usize, y: usize| {
        let off = (y * 64 + x) * 4;
        (out[off], out[off + 1], out[off + 2], out[off + 3])
    };
    for y in 5..25 {
        for x in 5..15 {
            let (b, g, r, _a) = pixel_at(x, y);
            assert_eq!(
                (b, g, r),
                (0xFF, 0xFF, 0xFF),
                "probe quad pixel ({x},{y}) not fully opaque — instance geometry likely \
                     sampled packed_w's atlas span instead of logical_w's: \
                     ({b:#x},{g:#x},{r:#x})",
            );
        }
    }
}

// ── Stage 3c.3 acceptance tests ─────────────────────────────
//
// Engine-direct RENDER paint oracles. Each test allocates one
// or two Vk-backed drawables, drives `render_composite` /
// `render_fill_rectangles` through `RenderEngine`, then
// round-trips via `get_image` and asserts pixel-level
// correctness against a CPU oracle. The seventh acceptance
// test (`render_composite_no_gc_clip_leak`) lives in
// `tests/acceptance.rs` because the "no GC clip leak"
// property is a Backend-trait invariant (engine has no GC
// clip notion).

/// Allocate a Vk-backed depth-32 pixmap and pre-fill it with
/// `color` via the engine's fill_rect path. Returns the
/// store DrawableId.
fn alloc_filled_pixmap(
    platform: &mut PlatformBackend,
    store: &mut DrawableStore,
    engine: &mut RenderEngine,
    xid: u32,
    w: u16,
    h: u16,
    color_bgra_premul: [f32; 4],
) -> DrawableId {
    let storage = platform
        .allocate_drawable_storage(w, h, 32)
        .expect("alloc storage");
    let id = store
        .allocate(
            xid,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("store.allocate");
    engine
        .fill_rect(
            store,
            platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
            },
            color_bgra_premul,
        )
        .expect("pre-fill");
    id
}

fn full_rect(w: u32, h: u32) -> crate::kms::vk::ops::render::CompositeRect {
    crate::kms::vk::ops::render::CompositeRect {
        src_x: 0,
        src_y: 0,
        mask_x: 0,
        mask_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: w,
        height: h,
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_over_renders_alpha_blended() {
    // 50%-alpha red (premultiplied: r=0.5, a=0.5) Over opaque
    // green. Over: out = src + dst * (1 - src.a).
    //   out.b = 0 + 0 * 0.5 = 0
    //   out.g = 0 + 1 * 0.5 = 0.5 → 0x80
    //   out.r = 0.5 + 0 * 0.5 = 0.5 → 0x80
    //   out.a = 0.5 + 1 * 0.5 = 1.0 → 0xFF
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 1.0, 0.0, 1.0], // opaque green
    );

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            3,                                           // Over
            ResolvedSource::Solid([0.5, 0.0, 0.0, 0.5]), // 50% red premul
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
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
        .expect("render_composite");
    assert_eq!(stats.recorded_draws, 1);
    assert!(!stats.used_dst_readback);
    assert!(!stats.used_src_alias_scratch);

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // Centre pixel (1, 1): BGRA = [0, 0x80, 0x80, 0xFF] (±1).
    let off = (4 + 1) * 4;
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    assert!(near(out[off], 0x00), "B at centre: got {:#x}", out[off]);
    assert!(
        near(out[off + 1], 0x80),
        "G at centre: got {:#x}",
        out[off + 1]
    );
    assert!(
        near(out[off + 2], 0x80),
        "R at centre: got {:#x}",
        out[off + 2]
    );
    assert!(
        near(out[off + 3], 0xFF),
        "A at centre: got {:#x}",
        out[off + 3]
    );

    engine.drain_all(&mut platform);
}

/// The cairo/Pango component-alpha text path, pass 1: glyph
/// coverage composited with `op=Add` into a depth-8 R8 a8 mask
/// pixmap (the i3-config-wizard black-dialog bug — this exact
/// shape was dropped by both the old `op != Over` gate and the
/// old BGRA8-only dst gate). Two half-coverage (0x80) Adds at
/// the same position must ACCUMULATE to full coverage —
/// distinguishing Add's `(ONE, ONE)` blend from Over, which
/// would converge on 0xC0.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_add_accumulates_into_r8_mask() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Depth-8 pixmap → R8_UNORM storage (format_for_depth).
    let storage = platform
        .allocate_drawable_storage(4, 4, 8)
        .expect("alloc a8 mask storage");
    let mask = store
        .allocate(
            0xA8A8,
            super::super::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .expect("store.allocate");
    assert_eq!(
        store.get(mask).unwrap().storage.format,
        vk::Format::R8_UNORM,
        "depth-8 pixmap must be R8 storage",
    );
    // Clear coverage to 0 (cairo FillRectangles op=Clear).
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(mask),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            [0.0, 0.0, 0.0, 0.0],
        )
        .expect("clear mask");

    // One 2×2 glyph of half coverage (0x80) at (1, 1), Added
    // twice. Opaque white premul foreground (cairo uses a
    // solid source for the mask pass): alpha = fg.a * cov.
    let pixels = [0x80u8; 4];
    let glyph = [CompositeGlyphInput {
        gs_xid: 0x6060,
        glyph_id: 7,
        w: 2,
        h: 2,
        pixels: GlyphPixels::A8(&pixels),
        dst_x: 1,
        dst_y: 1,
    }];
    for _ in 0..2 {
        engine
            .composite_glyphs(
                &mut store,
                &mut platform,
                Dst::server_internal(mask),
                12, // Add — the cairo mask-accumulation op
                0,  // pict_format unknown → depth heuristic (R8 ⇒ has-alpha)
                [1.0, 1.0, 1.0, 1.0],
                &glyph,
                None,
            )
            .expect("composite_glyphs Add");
    }

    // get_image closes the open frame and reads back. Depth-8
    // readback is 1 byte/pixel from the R channel.
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(mask),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            8,
        )
        .expect("get_image");
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    // Glyph pixel (1,1): 0x80 + 0x80 → 0xFF (clamped). Over
    // would give 0x80 + 0x80·(1−0.5) = 0xC0 — the assert
    // fails under Over, passes under Add.
    let at = |x: usize, y: usize| out[y * 4 + x];
    assert!(
        near(at(1, 1), 0xFF),
        "Add must accumulate coverage: got {:#x}",
        at(1, 1)
    );
    // Outside the glyph: still 0.
    assert!(
        near(at(0, 0), 0x00),
        "untouched mask pixel must stay 0: got {:#x}",
        at(0, 0)
    );

    engine.drain_all(&mut platform);
}

/// The cairo/Pango component-alpha text path, end to end:
/// pass 1 Adds glyph coverage into the a8 mask (above), pass 2
/// paints the window through the mask with the general
/// `Composite op=Src` (solid source, mask = the a8 pixmap —
/// sampled via the AlphaOnlyR8 swizzle). Text pixels must land
/// on the BGRA dst; zero-coverage pixels get src·0.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_add_mask_then_composite_src_renders_text() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Pass 1: a8 mask with a full-coverage 2×2 glyph at (1,1).
    let storage = platform
        .allocate_drawable_storage(4, 4, 8)
        .expect("alloc a8 mask storage");
    let mask = store
        .allocate(
            0xA8A9,
            super::super::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .expect("store.allocate");
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(mask),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            [0.0, 0.0, 0.0, 0.0],
        )
        .expect("clear mask");
    let pixels = [0xFFu8; 4];
    let glyph = [CompositeGlyphInput {
        gs_xid: 0x6061,
        glyph_id: 8,
        w: 2,
        h: 2,
        pixels: GlyphPixels::A8(&pixels),
        dst_x: 1,
        dst_y: 1,
    }];
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(mask),
            12, // Add
            0,
            [1.0, 1.0, 1.0, 1.0],
            &glyph,
            None,
        )
        .expect("composite_glyphs Add");

    // Pass 2: opaque-blue BGRA dst; Composite Src (white solid
    // through the mask) — the wizard's mask-paint pass.
    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x2,
        4,
        4,
        [0.0, 0.0, 1.0, 1.0], // opaque blue (premul RGBA)
    );
    engine
        .render_composite(
            &mut store,
            &mut platform,
            1,                                           // Src
            ResolvedSource::Solid([1.0, 1.0, 1.0, 1.0]), // opaque white
            ResolvedSource::Drawable(SourceDrawable::whole(mask)),
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
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
        .expect("render_composite Src through a8 mask");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
    // Glyph pixel (1,1): white·1 replaces blue → BGRA FF FF FF FF.
    let off = (4 + 1) * 4;
    assert!(
        near(out[off], 0xFF) && near(out[off + 1], 0xFF) && near(out[off + 2], 0xFF),
        "text pixel must be white: got BGR {:#x} {:#x} {:#x}",
        out[off],
        out[off + 1],
        out[off + 2],
    );
    // Zero-coverage pixel (3,3): Src ⇒ white·0 = transparent
    // black replaces blue.
    let off00 = (4 * 3 + 3) * 4;
    assert!(
        near(out[off00], 0x00) && near(out[off00 + 1], 0x00) && near(out[off00 + 2], 0x00),
        "zero-coverage pixel must be src·0: got BGR {:#x} {:#x} {:#x}",
        out[off00],
        out[off00 + 1],
        out[off00 + 2],
    );

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_picture_clip_per_rect() {
    // Two disjoint clip rects with a hole between them; one
    // composite covering the union bbox must paint inside both
    // rects AND leave the hole untouched. Exercises plan §4's
    // per-rect scissoring against v1's union-bbox shortcut.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        8,
        4,
        [0.0, 0.0, 1.0, 1.0], // RGBA: opaque blue
    );
    // Two clip rects with a 2-wide hole at x=3..=4.
    let clip = vec![
        Rectangle16 {
            x: 0,
            y: 0,
            width: 3,
            height: 4,
        },
        Rectangle16 {
            x: 5,
            y: 0,
            width: 3,
            height: 4,
        },
    ];
    engine
        .render_composite(
            &mut store,
            &mut platform,
            1,                                           // Src
            ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]), // RGBA: opaque red
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(8, 4)],
            Some(&clip),
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite");
    // Verify observable output: red inside both clip rects, original
    // blue preserved in the 2-wide hole. (The internal `recorded_draws`
    // count is an implementation detail of the pre-rework submit path
    // and is intentionally not asserted — the pixels are the contract.)
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // BGRA layout: B at +0, R at +2.
    for y in 0..4 {
        for x in 0..8u32 {
            let off = (y * 8 + x as usize) * 4;
            let in_clip = (0..3).contains(&x) || (5..8).contains(&x);
            if in_clip {
                assert_eq!(out[off + 2], 0xFF, "R painted at ({x},{y})");
                assert_eq!(out[off], 0x00, "B cleared at ({x},{y})");
            } else {
                // Hole (x=3..=4): original blue.
                assert_eq!(out[off], 0xFF, "B preserved at ({x},{y})");
                assert_eq!(out[off + 2], 0x00, "R untouched at ({x},{y})");
            }
        }
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_solid_fill_source_path() {
    // SolidFill source over (op=Src) an unrelated start colour —
    // every dst pixel must equal the source's premul colour.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 0.0, 0.0, 1.0], // opaque black
    );
    engine
        .render_composite(
            &mut store,
            &mut platform,
            1,                                             // Src
            ResolvedSource::Solid([0.25, 0.5, 0.75, 1.0]), // RGBA premul
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
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
        .expect("render_composite");
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // Storage BGRA bytes for RGBA(0.25, 0.5, 0.75, 1.0):
    // B=0.75→0xC0, G=0.5→0x80, R=0.25→0x40, A=1→0xFF.
    let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
    for px in out.chunks_exact(4) {
        assert!(near(px[0], 0xC0), "B: {:#x}", px[0]);
        assert!(near(px[1], 0x80), "G: {:#x}", px[1]);
        assert!(near(px[2], 0x40), "R: {:#x}", px[2]);
        assert!(near(px[3], 0xFF), "A: {:#x}", px[3]);
    }
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_linear_gradient_horizontal_two_stop() {
    // 256×1 dst pre-filled black; Composite Src + LinearGradient
    // source (p1=(0,0), p2=(256,0)<<16) with two stops:
    //   pos=0   black (0,0,0,1)
    //   pos=0xFFFFFFFF white (1,1,1,1)
    // Stage 3f.13 wires the LUT path — pixel n should read
    // roughly (n, n, n, 0xFF) ± a couple of units (NEAREST
    // sampler + LUT rounding).
    use crate::kms::vk::gradient::Stop;
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        256,
        1,
        [0.0, 0.0, 0.0, 1.0],
    );

    let grad_xid = 0xABBA_FACE_u32;
    engine
        .build_and_insert_linear_gradient(
            &mut platform,
            grad_xid,
            (0, 0),
            (256_i32 << 16, 0),
            &[
                Stop {
                    pos: 0,
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0xFFFF,
                },
                // 16.16 fixed-point: 1.0 = 0x10000. Using i32::MAX
                // here would put the second stop far past t=1.0,
                // so `sample_stops` would lerp `(target - 0) /
                // i32::MAX ≈ 0` and every LUT pixel would read
                // the first stop (black).
                Stop {
                    pos: 0x10000,
                    r: 0xFFFF,
                    g: 0xFFFF,
                    b: 0xFFFF,
                    a: 0xFFFF,
                },
            ],
        )
        .expect("build gradient");

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1, // Src — copy source to dst, no blend
            ResolvedSource::Gradient(grad_xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(256, 1)],
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
        .expect("render_composite gradient");
    assert_eq!(stats.recorded_draws, 1);

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 256,
                    height: 1,
                },
            },
            32,
        )
        .expect("get_image");

    // Sample several points along the ramp; tolerate ±4 due to
    // NEAREST sampler + 8-bit LUT quantisation + premultiplied
    // colour conversion. Direction-of-travel + monotonicity is
    // the strong gate (rules out the 3f.12 first-stop collapse,
    // which would read 0 at every x).
    let bgra = |x: usize| (out[x * 4], out[x * 4 + 1], out[x * 4 + 2], out[x * 4 + 3]);
    let (b0, g0, r0, _a0) = bgra(0);
    let (bm, gm, rm, _am) = bgra(128);
    let (b255, g255, r255, _a255) = bgra(255);
    // x=0 is near-black; x=255 is near-white; x=128 sits between.
    assert!(b0 <= 4 && g0 <= 4 && r0 <= 4, "x=0 BGRA={:?}", bgra(0));
    assert!(
        b255 >= 0xF0 && g255 >= 0xF0 && r255 >= 0xF0,
        "x=255 BGRA={:?}",
        bgra(255),
    );
    assert!(
        (0x40..=0xC0).contains(&bm) && (0x40..=0xC0).contains(&gm) && (0x40..=0xC0).contains(&rm),
        "x=128 BGRA={:?} (expected mid-grey)",
        bgra(128),
    );

    // Cleanup so the gradient image is freed in this drain.
    engine.picture_paint_remove(grad_xid);
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_radial_gradient_centred() {
    // 64×64 dst, radial gradient centred at (32,32) inner_r=0
    // outer_r=32, stops black→white. Center pixel should be
    // dark (t near 0 = first stop = black); border pixel should
    // be near-white.
    use crate::kms::vk::gradient::Stop;
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        64,
        64,
        [0.5, 0.5, 0.5, 1.0],
    );

    let grad_xid = 0xDEAD_BEEF_u32;
    engine
        .build_and_insert_radial_gradient(
            &mut platform,
            grad_xid,
            (32_i32 << 16, 32_i32 << 16, 0),
            (32_i32 << 16, 32_i32 << 16, 32_i32 << 16),
            &[
                Stop {
                    pos: 0,
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0xFFFF,
                },
                // 16.16 fixed-point: 1.0 = 0x10000. See linear-
                // gradient test above for why i32::MAX is wrong.
                Stop {
                    pos: 0x10000,
                    r: 0xFFFF,
                    g: 0xFFFF,
                    b: 0xFFFF,
                    a: 0xFFFF,
                },
            ],
        )
        .expect("build radial");

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1, // Src
            ResolvedSource::Gradient(grad_xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(64, 64)],
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
        .expect("render_composite radial");
    assert_eq!(stats.recorded_draws, 1);

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 64,
                    height: 64,
                },
            },
            32,
        )
        .expect("get_image");

    let bgra = |x: usize, y: usize| {
        let off = (y * 64 + x) * 4;
        (out[off], out[off + 1], out[off + 2], out[off + 3])
    };
    // Centre near-black, edge near-white.
    let (bc, gc, rc, _ac) = bgra(32, 32);
    assert!(
        bc < 0x40 && gc < 0x40 && rc < 0x40,
        "centre BGRA={:?} (expected dark)",
        bgra(32, 32),
    );
    // Corner is outside the unit circle for an inscribed
    // radial — pick a point on the rim instead (x=62, y=32 →
    // r ≈ 30/32).
    let (be, ge, re_, _ae) = bgra(62, 32);
    assert!(
        be > 0xC0 && ge > 0xC0 && re_ > 0xC0,
        "rim BGRA={:?} (expected near-white)",
        bgra(62, 32),
    );

    engine.picture_paint_remove(grad_xid);
    engine.drain_all(&mut platform);
}

/// #214: black → white two-stop ramp over x ∈ [0, 256).
fn bw_ramp_stops() -> [crate::kms::vk::gradient::Stop; 2] {
    use crate::kms::vk::gradient::Stop;
    [
        Stop {
            pos: 0,
            r: 0,
            g: 0,
            b: 0,
            a: 0xFFFF,
        },
        Stop {
            pos: 0x10000,
            r: 0xFFFF,
            g: 0xFFFF,
            b: 0xFFFF,
            a: 0xFFFF,
        },
    ]
}

/// #214: Src-composite gradient `xid` over the 256×1 `dst`, read it
/// back and check the ramp (black at 0, mid-grey at 128, white at
/// 255) — an uninitialized or not-yet-uploaded LUT fails this.
fn composite_and_check_bw_ramp(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    dst: DrawableId,
    xid: u32,
) {
    let stats = engine
        .render_composite(
            store,
            platform,
            1, // Src
            ResolvedSource::Gradient(xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(256, 1)],
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
        .expect("render_composite gradient");
    assert_eq!(stats.recorded_draws, 1);
    let out = engine
        .get_image(
            store,
            platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 256,
                    height: 1,
                },
            },
            32,
        )
        .expect("get_image");
    let px = |x: usize| [out[x * 4], out[x * 4 + 1], out[x * 4 + 2], out[x * 4 + 3]];
    assert!(px(0)[..3].iter().all(|&c| c <= 4), "x=0 BGRA={:?}", px(0));
    assert!(
        px(255)[..3].iter().all(|&c| c >= 0xF0),
        "x=255 BGRA={:?}",
        px(255)
    );
    assert!(
        px(128)[..3].iter().all(|&c| (0x40..=0xC0).contains(&c)),
        "x=128 BGRA={:?}",
        px(128)
    );
    assert!((0..256).all(|x| px(x)[3] == 0xFF), "alpha must be opaque");
}

fn close_for_tests(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
) {
    engine
        .close_open_frame(
            store,
            platform,
            super::super::frame_builder::CloseReason::Timeout,
        )
        .expect("close frame");
}

/// #214: CreateLinearGradient must not submit + wait; the upload is
/// queued on the open frame. A picture freed before any use stays
/// alive until the frame carrying its upload retires, then releases.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gradient_create_then_free_before_use_defers_release_to_frame_retire() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let xid = 0x0214_0001_u32;
    engine
        .build_and_insert_linear_gradient(
            &mut platform,
            xid,
            (0, 0),
            (256_i32 << 16, 0),
            &bw_ramp_stops(),
        )
        .expect("build gradient");
    let weak = {
        let inner = engine.inner.as_ref().expect("inner");
        let open = inner
            .frame_builder
            .open
            .as_ref()
            .expect("upload opens a frame");
        assert_eq!(open.gradient_inits.len(), 1, "upload queued, not run");
        assert!(open.ops.is_empty());
        match inner.picture_paint.get(&xid) {
            Some(PicturePaintState::Gradient(g)) => g.downgrade(),
            None => panic!("gradient not registered"),
        }
    };
    engine.picture_paint_remove(xid);
    assert!(
        weak.upgrade().is_some(),
        "image freed while its upload is still unsubmitted"
    );
    close_for_tests(&mut engine, &mut store, &mut platform);
    assert!(
        weak.upgrade().is_some(),
        "image freed while its upload may be in flight"
    );
    engine.drain_all(&mut platform);
    assert!(
        weak.upgrade().is_none(),
        "gradient resources leaked past frame retirement"
    );
}

/// #214: create, composite and free in ONE frame — the upload is
/// emitted at the frame head, ahead of the sampling op.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gradient_create_composite_free_in_one_frame_renders_and_releases() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        256,
        1,
        [1.0, 0.0, 0.0, 1.0],
    );
    // The pixmap fill must not share the frame: prove the gradient
    // itself opens (or joins) a frame and is ordered inside it.
    close_for_tests(&mut engine, &mut store, &mut platform);
    let xid = 0x0214_0002_u32;
    engine
        .build_and_insert_linear_gradient(
            &mut platform,
            xid,
            (0, 0),
            (256_i32 << 16, 0),
            &bw_ramp_stops(),
        )
        .expect("build gradient");
    let weak = match engine
        .inner
        .as_ref()
        .expect("inner")
        .picture_paint
        .get(&xid)
    {
        Some(PicturePaintState::Gradient(g)) => g.downgrade(),
        None => panic!("gradient not registered"),
    };
    // Record the composite, then free the picture while the frame is
    // still open; get_image closes and submits that frame.
    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1,
            ResolvedSource::Gradient(xid),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(256, 1)],
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
        .expect("render_composite gradient");
    assert_eq!(stats.recorded_draws, 1);
    {
        let open = engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .open
            .as_ref()
            .expect("open");
        assert_eq!(open.gradient_inits.len(), 1);
        assert_eq!(open.ops.len(), 1, "composite shares the upload's frame");
    }
    engine.picture_paint_remove(xid);
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 256,
                    height: 1,
                },
            },
            32,
        )
        .expect("get_image");
    let px = |x: usize| [out[x * 4], out[x * 4 + 1], out[x * 4 + 2]];
    assert!(px(0).iter().all(|&c| c <= 4), "x=0 BGR={:?}", px(0));
    assert!(
        px(255).iter().all(|&c| c >= 0xF0),
        "x=255 BGR={:?}",
        px(255)
    );
    assert!(
        px(128).iter().all(|&c| (0x40..=0xC0).contains(&c)),
        "x=128 BGR={:?}",
        px(128)
    );
    engine.drain_all(&mut platform);
    assert!(
        weak.upgrade().is_none(),
        "gradient resources leaked past frame retirement"
    );
}

/// #214: a gradient whose upload frame was already submitted is
/// sampled correctly by a LATER frame (same-queue order + the
/// upload's closing barrier), and many creates stay correct.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gradient_sampled_in_a_later_frame_sees_the_upload() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        256,
        1,
        [1.0, 0.0, 0.0, 1.0],
    );
    // A burst of creates in one frame (a GTK repaint), then use one.
    for i in 0..32_u32 {
        engine
            .build_and_insert_linear_gradient(
                &mut platform,
                0x0214_0100 + i,
                (0, 0),
                (256_i32 << 16, 0),
                &bw_ramp_stops(),
            )
            .expect("build gradient");
    }
    close_for_tests(&mut engine, &mut store, &mut platform);
    composite_and_check_bw_ramp(
        &mut engine,
        &mut store,
        &mut platform,
        dst,
        0x0214_0100 + 17,
    );
    for i in 0..32_u32 {
        engine.picture_paint_remove(0x0214_0100 + i);
    }
    assert_eq!(engine.picture_paint_len(), 0);
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_missing_gradient_picture_is_gap() {
    // Engine receives a ResolvedSource::Gradient(xid) for an
    // xid that has no picture_paint entry (LUT build failed or
    // dropped early). Must return stats with recorded_draws=0,
    // log a debug gap, and NOT panic.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 0.0, 0.0, 1.0],
    );

    let stats = engine
        .render_composite(
            &mut store,
            &mut platform,
            1, // Src
            ResolvedSource::Gradient(0xC0FF_EE00),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
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
        .expect("render_composite Ok even on missing gradient");
    assert_eq!(stats.recorded_draws, 0);
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_composite_self_alias() {
    // src == dst: pre-fill with a vertical gradient, then
    // Composite(Over, dst, NoMask, dst). Over with itself on
    // opaque alpha yields self exactly (out = src + dst*(1-1) =
    // src). Without the scratch path the GPU samples a region
    // as it writes it — undefined behaviour; with it, the
    // result must be bit-identical to the pre-fill.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // Allocate + PutImage a distinct pattern (per-pixel unique).
    let storage = platform.allocate_drawable_storage(8, 4, 32).expect("alloc");
    let dst = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("alloc");
    let mut src_bytes = vec![0u8; 8 * 4 * 4];
    for y in 0u8..4 {
        for x in 0u8..8 {
            let off = (usize::from(y) * 8 + usize::from(x)) * 4;
            src_bytes[off] = x * 0x20; // B
            src_bytes[off + 1] = y * 0x40; // G
            src_bytes[off + 2] = (x + y) * 0x10; // R
            src_bytes[off + 3] = 0xFF; // A (opaque)
        }
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(dst),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 8,
                height: 4,
            },
            &src_bytes,
            32,
        )
        .expect("put_image");

    engine
        .render_composite(
            &mut store,
            &mut platform,
            3, // Over
            ResolvedSource::Drawable(SourceDrawable::whole(dst)),
            ResolvedSource::None,
            Dst::server_internal(dst),
            &[full_rect(8, 4)],
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
        .expect("render_composite");
    // The real contract: Over(self, NoMask, self) on opaque alpha must
    // be bit-identical to self — i.e. the engine must NOT let the GPU
    // sample dst while writing it (read-write hazard → corruption). We
    // assert that on the observable output below rather than on the
    // internal `used_src_alias_scratch` routing flag (an implementation
    // detail of how the hazard is avoided).
    let after = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    assert_eq!(
        after, src_bytes,
        "Over(self, NoMask, self) must equal self bit-identical",
    );

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_fill_rectangles_src_clears_to_color() {
    // render_fill_rectangles(op=Src, premul colour) — every
    // pixel in the rect must equal the premul colour.
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let dst = alloc_filled_pixmap(
        &mut platform,
        &mut store,
        &mut engine,
        0x1,
        4,
        4,
        [0.0, 0.0, 0.0, 1.0],
    );
    let stats = engine
        .render_fill_rectangles(
            &mut store,
            &mut platform,
            1,                    // Src
            [1.0, 0.0, 0.0, 1.0], // RGBA: opaque red premul
            Dst::server_internal(dst),
            &[full_rect(4, 4)],
            None,
        )
        .expect("render_fill_rectangles");
    assert_eq!(stats.recorded_draws, 1);
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // BGRA: B=0, G=0, R=0xFF, A=0xFF.
    for px in out.chunks_exact(4) {
        assert_eq!(&px[..4], &[0x00, 0x00, 0xFF, 0xFF]);
    }
    engine.drain_all(&mut platform);
}

// ── Stage 3e.2 decoder + degenerate-trap unit tests ─────────

/// Per plan §3e: round-trip a known wire bytestream through
/// the trapezoid decoder. Verifies field offsets + 16.16
/// fixed-point interpretation. Uses the same shape as v1's
/// `try_vk_render_trapezoids_path` (kms/backend.rs:4286)
/// since v2's `render_trapezoids` mirrors that decoder.
#[test]
fn trapezoid_decoder_x11_wire_layout() {
    // Build a single trapezoid wire record: 10 i32 fields, 40
    // bytes. Field order: top, bottom, left_p1.x, left_p1.y,
    // left_p2.x, left_p2.y, right_p1.x, right_p1.y,
    // right_p2.x, right_p2.y. All values are 16.16 fixed-point.
    let mut wire: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        0,        // top = 0.0
        10 << 16, // bottom = 10.0
        2 << 16,  // left_p1.x = 2.0
        0,        // left_p1.y = 0.0
        2 << 16,  // left_p2.x = 2.0
        10 << 16, // left_p2.y = 10.0
        8 << 16,  // right_p1.x = 8.0
        0,        // right_p1.y = 0.0
        8 << 16,  // right_p2.x = 8.0
        10 << 16, // right_p2.y = 10.0
    ];
    for v in fields {
        wire.extend_from_slice(&v.to_le_bytes());
    }

    // Decode mirroring the backend's `render_trapezoids` body.
    let chunk: &[u8] = &wire;
    let read_i32 = |o: usize| -> i32 {
        i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
    };
    let trap = crate::kms::vk::ops::traps::Trapezoid {
        top: read_i32(0),
        bottom: read_i32(4),
        left_p1: (read_i32(8), read_i32(12)),
        left_p2: (read_i32(16), read_i32(20)),
        right_p1: (read_i32(24), read_i32(28)),
        right_p2: (read_i32(32), read_i32(36)),
    };
    assert_eq!(trap.top, 0);
    assert_eq!(trap.bottom, 10 << 16);
    assert_eq!(trap.left_p1, (2 << 16, 0));
    assert_eq!(trap.left_p2, (2 << 16, 10 << 16));
    assert_eq!(trap.right_p1, (8 << 16, 0));
    assert_eq!(trap.right_p2, (8 << 16, 10 << 16));

    // bbox: x ∈ [2, 8], y ∈ [0, 10]; integer = (2, 0, 8, 10).
    let bbox =
        crate::kms::vk::ops::traps::trapezoid_bbox(&[trap]).expect("bbox for non-degenerate trap");
    assert_eq!(bbox, (2, 0, 8, 10));
}

/// Per plan §3e: each Triangle's three vertices round-trip
/// through the wire decoder, and the bbox helper hits each
/// vertex (so a degenerate triangle — three colinear points —
/// still produces a finite bbox if the points span pixels).
/// Mirrors v1's `try_vk_render_triangles_path` decoder shape.
#[test]
fn triangle_to_trap_degenerate() {
    let tri = crate::kms::vk::ops::traps::Triangle {
        p1: (0, 0),
        p2: (4 << 16, 0),
        p3: (2 << 16, 8 << 16),
    };
    let inst = tri.to_instance_data();
    assert!((inst.p1[0] - 0.0).abs() < 1e-6);
    assert!((inst.p2[0] - 4.0).abs() < 1e-6);
    assert!((inst.p3[1] - 8.0).abs() < 1e-6);
    let bbox = crate::kms::vk::ops::traps::triangle_bbox(&[tri])
        .expect("bbox for non-degenerate triangle");
    assert_eq!(bbox, (0, 0, 4, 8));

    // Degenerate (three colinear points) — bbox helper still
    // returns Some(extents) because the points span the axes.
    // What v1 + v2 do with such an input is: GPU pipeline draws
    // a zero-area triangle (no pixels covered), CB safely
    // completes. The plan's "degenerate trap" phrasing refers
    // to the encoding (trap with one zero-length edge), not a
    // helper output — the test confirms the trivial bbox path
    // doesn't choke on it.
    let colinear = crate::kms::vk::ops::traps::Triangle {
        p1: (0, 0),
        p2: (4 << 16, 0),
        p3: (8 << 16, 0),
    };
    assert!(crate::kms::vk::ops::traps::triangle_bbox(&[colinear]).is_none());
}

/// Stage 3f.15: `fill_rect_batch` records N rects into ONE CB +
/// ONE submit + ONE `SubmittedOp`. Drives 3 disjoint rects on a
/// 16×4 BGRA8 dst pre-cleared to blue, fills them red, and
/// asserts (a) the dst observes red inside each rect and blue
/// outside, and (b) `inner.submitted` grew by exactly 1 across the
/// two fill calls (blue-prefill + red-batch) after the frame closes.
///
/// Phase B.3 update: fill_rect / fill_rect_batch now append to the
/// open frame instead of submitting immediately. The count assertion
/// is now gated on closing the frame first (via
/// `close_open_frame_for_timeout_for_tests`), then asserting submitted
/// grew by the expected count. The pixel-correctness assertions are
/// unchanged — `get_image` closes any open frame internally (via
/// `close_open_frame(SyncWait)`) so they still observe all fills.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_rect_batch_one_submit_for_n_rects() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(16, 4, 32)
        .expect("alloc");
    let id = store
        .allocate(
            0x1,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // Pre-fill the dst with blue so we can see the batch-painted
    // rects against a known background. Phase B.3: this now appends
    // to the open frame instead of submitting a per-op CB.
    let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 16,
                    height: 4,
                },
            },
            blue,
        )
        .expect("blue prefill");

    // Close the blue-prefill frame so the red-batch starts in a
    // fresh frame. This mirrors the production sequence where
    // fill_rect is followed by a different op that closes the frame.
    // After close + flush, the blue-prefill SubmittedOp is in submitted.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close blue-prefill frame");
    engine
        .flush_submit_group(
            &mut store,
            &mut platform,
            super::super::submit_group::FlushReason::SyncBoundary,
        )
        .expect("setup flush");

    // Snapshot the SubmittedOp count BEFORE the red batch so we
    // can assert exactly +1 (the red frame) across the call.
    let before = engine
        .inner
        .as_ref()
        .map(|i| i.submitted.len())
        .unwrap_or(0);

    let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
    let rects = [
        vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 2,
                height: 2,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 6, y: 1 },
            extent: vk::Extent2D {
                width: 3,
                height: 2,
            },
        },
        vk::Rect2D {
            offset: vk::Offset2D { x: 13, y: 2 },
            extent: vk::Extent2D {
                width: 3,
                height: 2,
            },
        },
    ];
    engine
        .fill_rect_batch(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            red,
            &rects,
        )
        .expect("fill_rect_batch");

    // Phase B.3: close the open frame (red batch) before asserting
    // the SubmittedOp count — the op is now frame-resident until close.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close red-batch frame");
    engine
        .flush_submit_group(
            &mut store,
            &mut platform,
            super::super::submit_group::FlushReason::SyncBoundary,
        )
        .expect("flush before count assertion");

    let after = engine
        .inner
        .as_ref()
        .map(|i| i.submitted.len())
        .unwrap_or(0);
    assert_eq!(
        after,
        before + 1,
        "fill_rect_batch (red rects) must produce exactly ONE SubmittedOp \
             regardless of rect count — N4 invariant (before={before}, after={after})"
    );

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 16,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");

    // Helper: does (x, y) fall inside any of the painted rects?
    let in_rect = |x: i32, y: i32| -> bool {
        rects.iter().any(|r| {
            x >= r.offset.x
                && y >= r.offset.y
                && x < r.offset.x + r.extent.width as i32
                && y < r.offset.y + r.extent.height as i32
        })
    };
    for y in 0..4 {
        for x in 0..16 {
            let off = (y * 16 + x) as usize * 4;
            let px = &out[off..off + 4];
            if in_rect(x, y) {
                assert_eq!(px[2], 0xFF, "rect pixel ({x},{y}) R should be 0xFF (red)");
                assert_eq!(px[0], 0x00, "rect pixel ({x},{y}) B should be 0x00");
            } else {
                assert_eq!(
                    px[0], 0xFF,
                    "background pixel ({x},{y}) B should be 0xFF (blue)"
                );
                assert_eq!(px[2], 0x00, "background pixel ({x},{y}) R should be 0x00");
            }
        }
    }

    engine.drain_all(&mut platform);
}

/// X11 Render PictFormat fix — resolver-level oracle.
///
/// Per the X11 Render spec, a Picture wrapping a depth-24
/// drawable has `PictFormat.alpha_mask = 0`; samples must
/// return α = 1.0 regardless of the storage's padding byte.
/// `resolve_force_opaque` is the single point where v2's
/// `render_composite` and `render_traps_or_tris` decide
/// whether to set the shader-side force-opaque bit on the
/// src/mask picture.
///
/// This test is the logic-only gate: a depth-24 Drawable
/// must resolve to `true`; depth-32 to `false`. Solid and
/// Gradient sources carry α intrinsically (LUT-baked or
/// caller-supplied), so they're always `false`. `None` is
/// the synthetic white-mask path — `α = 1.0` already by
/// construction, so no override needed.
#[test]
fn render_composite_resolve_force_opaque_oracle() {
    let mut store = DrawableStore::new();
    let storage32 = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id32 = store
        .allocate(
            0xA001,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage32,
        )
        .unwrap();
    let storage24 = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id24 = store
        .allocate(
            0xA002,
            super::super::store::DrawableKind::Pixmap,
            24,
            false,
            storage24,
        )
        .unwrap();

    // depth-32 Drawable: storage's α byte is client-meaningful,
    // do not force.
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id32))
    ));
    // depth-24 Drawable: storage's α byte is server-owned
    // padding, force α = 1.0.
    assert!(resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id24))
    ));

    // Solid: α is caller-supplied premul. Gradient: α is
    // LUT-baked. None: white-mask scratch is initialised to
    // α = 1.0 at engine init. All three pass through.
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Solid([1.0, 0.0, 0.0, 1.0]),
    ));
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Gradient(0x1234)
    ));
    assert!(!resolve_force_opaque(&store, &ResolvedSource::None));

    // depth-1 (bitmap mask) and depth-8 (a8 alpha picture)
    // both have meaningful α in their PictFormat — α carries
    // the bitmap value / coverage. Forcing α = 1.0 on those
    // would turn coverage masks into solid blocks, so the
    // resolver explicitly excludes them. Only depth-24 (the
    // x8r8g8b8 / r8g8b8 case where storage's α byte is
    // server-owned padding) gets the override.
    let storage1 = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id1 = store
        .allocate(
            0xA003,
            super::super::store::DrawableKind::Pixmap,
            1,
            false,
            storage1,
        )
        .unwrap();
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id1))
    ));
    let storage8 = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let id8 = store
        .allocate(
            0xA004,
            super::super::store::DrawableKind::Pixmap,
            8,
            false,
            storage8,
        )
        .unwrap();
    assert!(!resolve_force_opaque(
        &store,
        &ResolvedSource::Drawable(SourceDrawable::whole(id8))
    ));
}

/// Audit #4 (2026-05-19) — `pict_format` overrides the depth
/// heuristic for `Drawable` sources. A picture wrapping a
/// depth-32 storage with `RENDER_FMT_XRGB32` declares
/// `alpha_mask=0` — the storage's α byte is padding, not
/// client-meaningful. Engine must force α=1 even though
/// `d.depth == 32`. Pre-fix `resolve_force_opaque` ignored
/// pict_format → depth-32 storages with xRGB32 sampled as
/// transparent black against the wallpaper.
#[test]
fn render_composite_resolve_force_opaque_honors_xrgb32_pict_format() {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    let mut store = DrawableStore::new();
    // Depth-32 storage (would normally sample with real α).
    let storage32 = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id32 = store
        .allocate(
            0xA101,
            super::super::store::DrawableKind::Pixmap,
            32,
            false,
            storage32,
        )
        .unwrap();
    // Depth-24 storage (α is padding regardless of pict_format).
    let storage24 = super::super::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id24 = store
        .allocate(
            0xA102,
            super::super::store::DrawableKind::Pixmap,
            24,
            false,
            storage24,
        )
        .unwrap();
    let src32 = ResolvedSource::Drawable(SourceDrawable::whole(id32));
    let src24 = ResolvedSource::Drawable(SourceDrawable::whole(id24));

    // pict_format=0 (no picture context) → fall back to depth
    // heuristic (the engine-internal callers that synthesize
    // sources pass 0 here).
    assert!(!resolve_force_opaque_pict_format(&store, &src32, 0));
    assert!(resolve_force_opaque_pict_format(&store, &src24, 0));

    // pict_format=RENDER_FMT_XRGB32 on depth-32 storage → force
    // opaque (the audit-#4 case). Pre-fix would have returned
    // false because depth==32.
    assert!(resolve_force_opaque_pict_format(
        &store,
        &src32,
        RENDER_FMT_XRGB32,
    ));
    // pict_format=RENDER_FMT_ARGB32 on depth-32 storage → use
    // storage α (current behavior preserved).
    assert!(!resolve_force_opaque_pict_format(
        &store,
        &src32,
        RENDER_FMT_ARGB32,
    ));
    // pict_format=RENDER_FMT_RGB24 on depth-24 storage → force
    // opaque (consistent with the legacy depth-24 path).
    assert!(resolve_force_opaque_pict_format(
        &store,
        &src24,
        RENDER_FMT_RGB24,
    ));
}

/// Audit #4 (2026-05-19) — destination `pict_format` overrides
/// the depth-32 storage heuristic. A Picture wrapping a
/// depth-32 storage with `RENDER_FMT_XRGB32` declares
/// `alpha_mask = 0` — the dst storage has no client-meaningful
/// alpha channel, padding bytes only. The engine must drive
/// the pipeline + readback selection as "no alpha target,"
/// matching the depth-24 case, otherwise post-composite reads
/// of those padding bytes leak through to subsequent samples
/// as partial transparency. Pre-fix `dst_has_alpha = depth == 32`
/// unconditionally → xRGB32 destination treated as ARGB.
#[test]
fn render_composite_dst_has_alpha_honors_xrgb32_pict_format() {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    // pict_format=0 (no picture context — engine-internal callers
    // synthesizing draws) → depth heuristic.
    assert!(!dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        24,
        0,
    ));
    assert!(dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        32,
        0,
    ));

    // XRGB32 on depth-32 storage → no alpha (audit #4 case).
    assert!(!dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        32,
        RENDER_FMT_XRGB32,
    ));
    // ARGB32 on depth-32 storage → use storage alpha
    // (current behavior preserved).
    assert!(dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        32,
        RENDER_FMT_ARGB32,
    ));
    // RGB24 on depth-24 storage → no alpha (consistent with
    // legacy depth-24 path).
    assert!(!dst_has_alpha_for_pict_format(
        vk::Format::B8G8R8A8_UNORM,
        24,
        RENDER_FMT_RGB24,
    ));
    // R8 storage (A8 mask destination) is alpha-only regardless
    // of pict_format — A8 destinations DO have alpha bytes.
    assert!(dst_has_alpha_for_pict_format(vk::Format::R8_UNORM, 8, 0));
}

/// Audit #4 — `swizzle_class_for` must pick `BgraNoAlpha`
/// (force α=ONE swizzle on the sample view) whenever the
/// picture's PictFormat declares `alpha_mask=0`, not just
/// when `depth == 24`. Pre-fix, depth-32 storages always got
/// `RgbaIdent` (pass-through), so an xRGB32 picture wrapping
/// a depth-32 storage with α=0 padding bytes sampled as
/// transparent.
#[test]
fn render_composite_swizzle_class_for_pict_format_xrgb32_is_no_alpha() {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};

    // pict_format=0 falls back to depth heuristic.
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 24, 0),
        SwizzleClass::BgraNoAlpha,
    );
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
        SwizzleClass::RgbaIdent,
    );

    // xRGB32 on depth-32 storage → BgraNoAlpha (force α=ONE).
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, RENDER_FMT_XRGB32,),
        SwizzleClass::BgraNoAlpha,
    );
    // ARGB32 on depth-32 storage → RgbaIdent (use storage α).
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, RENDER_FMT_ARGB32,),
        SwizzleClass::RgbaIdent,
    );
    // RGB24 on depth-24 storage → BgraNoAlpha (already true via
    // depth, preserved when pict_format aligns).
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::B8G8R8A8_UNORM, 24, RENDER_FMT_RGB24,),
        SwizzleClass::BgraNoAlpha,
    );
    // R8 storage (A8 mask) is alpha-only regardless of pict_format.
    assert_eq!(
        swizzle_class_for_pict_format(vk::Format::R8_UNORM, 8, 0),
        SwizzleClass::AlphaOnlyR8,
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn engine_exposes_descriptor_pool_ring_lifetime_counters() {
    let b = match super::super::backend::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    assert_eq!(b.engine.descriptor_pool_creates_lifetime(), 0);
    assert_eq!(b.engine.descriptor_pool_resets_lifetime(), 0);
}

// ── Task 3 Phase A regression tests ─────────────────────────

// ── Task 4 Phase A regression tests ─────────────────────────

// ── Task 7 Phase A regression tests ─────────────────────────

// ────────────────────────────────────────────────────────────
// Phase B.2 Task 4: overlay-as-source-of-truth read accessor
// + commit_close_success overlay → storage write-back.
// ────────────────────────────────────────────────────────────

/// `RenderEngineInner::current_layout_for_drawable` returns the
/// overlay's `current_in_frame_layout` once the drawable has been
/// first-touched and updated in-frame; falls back to
/// `storage.current_layout` when no frame is open / drawable
/// untouched.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn current_layout_for_drawable_reads_overlay_when_first_touched() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let id = engine
        .create_pixmap(&mut store, &mut platform, 0x4f00_0001, 8, 8, 32)
        .expect("create");

    // Storage seeded with UNDEFINED by `allocate_drawable_storage`.
    // Pre-condition (no frame open): wrapper returns the storage
    // value directly.
    {
        let inner = engine.inner.as_ref().expect("inner");
        assert_eq!(
            inner.current_layout_for_drawable(&store, id),
            vk::ImageLayout::UNDEFINED,
            "no frame open + UNDEFINED storage → wrapper returns UNDEFINED",
        );
    }

    // Open a frame and first-touch the drawable, then update its
    // in-frame layout to COLOR_ATTACHMENT_OPTIMAL — same shape a
    // ported `render_composite` will use at op-append time.
    let ticket = platform
        .submit_group_ticket_or_open()
        .expect("submit_group_ticket_or_open");
    engine.open_frame_for_paint_for_tests(ticket);
    {
        let inner = engine.inner.as_mut().expect("inner");
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.layouts
            .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
        open.layouts
            .set_drawable_in_frame(id, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    }

    // Wrapper now consults the overlay — must see the in-frame
    // value, NOT the (still UNDEFINED) storage value.
    {
        let inner = engine.inner.as_ref().expect("inner");
        assert_eq!(
            inner.current_layout_for_drawable(&store, id),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            "frame open + drawable first-touched → wrapper returns overlay's \
                 current_in_frame_layout (overlay-as-source-of-truth invariant)",
        );
    }
    // Storage unchanged during recording.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::UNDEFINED,
        "storage NOT mutated during recording (B.2 invariant)",
    );

    // Untouched second drawable in the same open frame falls
    // through to its storage layout.
    let id2 = engine
        .create_pixmap(&mut store, &mut platform, 0x4f00_0002, 8, 8, 32)
        .expect("create #2");
    {
        let inner = engine.inner.as_ref().expect("inner");
        assert_eq!(
            inner.current_layout_for_drawable(&store, id2),
            vk::ImageLayout::UNDEFINED,
            "untouched drawable in open frame → wrapper falls back to storage",
        );
    }

    // Close the frame cleanly so drop-time invariants hold.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close");
    engine.drain_all(&mut platform);
}

/// `commit_close_success` writes each touched drawable's
/// `current_in_frame_layout` back to `storage.current_layout`
/// (USER-codex U-R6.F1 — LOAD-BEARING).
///
/// Without this commit, a B.2 frame ports that route layout
/// transitions exclusively through the overlay would leave
/// `Drawable::storage.current_layout` stale after submit — the
/// next op (legacy or ported) would emit a barrier from the wrong
/// `old_layout`, corrupting / device-losing on the next render.
///
/// This unit test substitutes for the integration test sketched
/// in the plan (Step 6) which depends on Task 5's
/// `set_frame_builder_render_composite_enabled_for_tests` gate +
/// Task 8's `render_composite_via_frame_builder` body — neither
/// has landed yet. The substitute exercises the commit path
/// directly: seed the overlay manually, drive the close, assert
/// storage caught up.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn commit_close_success_writes_overlay_into_storage() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let id = engine
        .create_pixmap(&mut store, &mut platform, 0x4f01_0001, 8, 8, 32)
        .expect("create");
    // Storage starts UNDEFINED.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::UNDEFINED,
    );

    // Open a frame, seed the overlay as a ported op would: first
    // touch records the pre-frame layout, then `set_*_in_frame`
    // captures the post-op exit layout. (For `render_composite`,
    // that's SHADER_READ_ONLY_OPTIMAL per Pitfall 6.)
    let ticket = platform
        .submit_group_ticket_or_open()
        .expect("submit_group_ticket_or_open");
    engine.open_frame_for_paint_for_tests(ticket);
    {
        let inner = engine.inner.as_mut().expect("inner");
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.layouts
            .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
        open.layouts
            .set_drawable_in_frame(id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
    // While the frame is open, storage MUST NOT have moved.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::UNDEFINED,
        "storage unchanged during recording (B.2 invariant)",
    );

    // Close on success — frame has no recorded ops, so the
    // empty CB submits cleanly and `commit_close_success` runs.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close");

    // Storage MUST have caught up to the overlay's in-frame value.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "commit_close_success wrote overlay → storage \
             (USER-codex U-R6.F1 LOAD-BEARING invariant)",
    );
    engine.drain_all(&mut platform);
}

/// RENDER `Trapezoids`/`Triangles` must honour the client's
/// `xSrc`/`ySrc` source origin when the source is a picture (e.g.
/// GTK CSD shadow blur-ramp masks sampled at `ySrc != 0`). The
/// trap composite hardcoded `src_x/src_y = 0`, collapsing the ramp
/// to a solid slab → opaque black bar below tooltips. This locks
/// the origin convention the emit now applies.
#[test]
fn trap_composite_src_origin_honours_xsrc_ysrc() {
    // full-dst branch (op=Src builds the A8 mask): the coverage
    // mask carries the bbox offset, so the source aligns directly
    // at the shifted client origin (ySrc=25 in the tooltip trace).
    assert_eq!(trap_composite_src_origin_axis(25, 18, true), 25);
    assert_eq!(trap_composite_src_origin_axis(0, 18, true), 0);
    // non-full-dst branch: composite renders at the bbox origin, so
    // the source adds it back (Xorg miTrapezoids: src at xSrc+dst).
    assert_eq!(trap_composite_src_origin_axis(25, 18, false), 43);
    // shifted-negative base (redirect/x_off pushed the origin left)
    // composes linearly with the bbox add.
    assert_eq!(trap_composite_src_origin_axis(-4, 10, false), 6);
    // The pre-fix behaviour (origin always 0) is now only correct
    // for a zero client origin on the full-dst path — proving the
    // hardcoded 0 was wrong for every nonzero xSrc/ySrc.
    assert_ne!(trap_composite_src_origin_axis(25, 18, true), 0);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn sampled_scratch_image_has_view_and_sampled_usage() {
    let Ok(vk) = crate::kms::vk::device::VkContext::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let vk = std::sync::Arc::new(vk);
    let s = super::allocate_sampled_scratch_image(&vk, 16, 8, ash::vk::Format::B8G8R8A8_UNORM)
        .expect("allocate sampled scratch");
    assert_ne!(
        s.view,
        ash::vk::ImageView::null(),
        "must expose an IDENTITY view"
    );
    assert!(s.size_bytes > 0);
}

// ── #137 step 1: pin-ceiling reservation for the instance buffer ──
//
// The pre-pass / per-glyph admission rules budget prospective glyph
// *uploads* only; the instance buffer is then pinned unconditionally
// afterwards. A call whose uploads exactly fill the declared ceiling
// therefore ends the frame at `ceiling + 1` pins. These assert the
// actual pin COUNT, never "it rendered" — the off-by-one renders
// fine, so an outcome-based test would pass on the broken code.

/// `composite_glyphs_via_frame_builder`'s half of the hole
/// (`render/engine.rs`, pre-pass / single-call-overflow / per-glyph
/// admission). Ceiling of 2, two never-before-seen glyphs in ONE
/// call: pre-fix, both upload (2 pins) and the instance buffer pins
/// unconditionally after (1 more) = 3 pins against a ceiling of 2.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn composite_glyphs_pin_ceiling_reserves_instance_buffer_pin() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

    {
        let inner = engine.inner.as_mut().expect("inner");
        inner.frame_builder.set_max_pinned_resources_per_frame(2);
    }

    let pixels_a = [0xFFu8; 4];
    let pixels_b = [0xFFu8; 4];
    let glyphs = [
        CompositeGlyphInput {
            gs_xid: 0x7001,
            glyph_id: 1,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(&pixels_a),
            dst_x: 1,
            dst_y: 1,
        },
        CompositeGlyphInput {
            gs_xid: 0x7001,
            glyph_id: 2,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(&pixels_b),
            dst_x: 10,
            dst_y: 1,
        },
    ];

    let stats = engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            3, // Over
            0, // pict_format unknown → depth heuristic
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
            None,
        )
        .expect("composite_glyphs");

    let inner = engine.inner.as_ref().expect("inner");
    let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame stays open after composite_glyphs");
    assert!(
        open.pins.len() <= ceiling,
        "pin set must never exceed the declared ceiling: {} pins against a \
             ceiling of {} (glyphs_dropped={})",
        open.pins.len(),
        ceiling,
        stats.glyphs_dropped,
    );

    engine.drain_all(&mut platform);
}

/// #177: an upload arena block must never be handed out again while
/// the GPU can still read it. A frame's blocks stay with the frame
/// while it is open and while it is closed but its fence has not
/// signalled (recorded into the submit group, not yet submitted); a
/// frame opened meanwhile gets a block of its own. Only the retire walk
/// after the fence signals returns the blocks, and later frames then
/// reuse them without allocating.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn upload_blocks_are_not_reused_while_their_frame_is_in_flight() {
    use crate::kms::vk::mem_accounting::thread_alloc_calls;
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let pixels = [0xFFu8; 4];
    let glyphs = [CompositeGlyphInput {
        gs_xid: 0x7001,
        glyph_id: 1,
        w: 2,
        h: 2,
        pixels: GlyphPixels::A8(&pixels),
        dst_x: 1,
        dst_y: 1,
    }];
    let draw =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .composite_glyphs(
                    store,
                    platform,
                    Dst::server_internal(target),
                    3, // Over
                    0,
                    [1.0, 1.0, 1.0, 1.0],
                    &glyphs,
                    None,
                )
                .expect("composite_glyphs");
        };
    let settle =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .flush_submit_group(
                    store,
                    platform,
                    super::super::submit_group::FlushReason::SyncBoundary,
                )
                .expect("flush");
            platform.wait_idle_bounded();
            engine.poll_retired(platform);
        };
    let idle = |engine: &RenderEngine| {
        engine
            .inner
            .as_ref()
            .expect("inner")
            .upload_arena
            .idle_len()
    };
    // The upload slices of the open frame.
    let open_slices = |engine: &RenderEngine| {
        engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .open
            .as_ref()
            .expect("open frame")
            .pins
            .upload_slices
            .clone()
    };

    // Warm-up: atlas, pipelines, the glyph's upload and one block, which
    // the retire walk leaves on the idle list.
    draw(&mut engine, &mut store, &mut platform);
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close warm-up frame");
    settle(&mut engine, &mut store, &mut platform);
    assert_eq!(idle(&engine), 1);

    // Frame A: two requests share the idle block; nothing is allocated.
    let before = thread_alloc_calls();
    draw(&mut engine, &mut store, &mut platform);
    draw(&mut engine, &mut store, &mut platform);
    assert_eq!(
        thread_alloc_calls() - before,
        0,
        "frame A reuses the idle block"
    );
    let a = open_slices(&engine);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].buffer, a[1].buffer, "one block for the frame");
    assert_eq!(a[0].offset, 0);
    assert!(
        a[1].offset > 0 && a[1].offset % UPLOAD_VERTEX_ALIGN == 0,
        "{a:?}"
    );
    assert_eq!(idle(&engine), 0);

    // Closing submits frame A, whose fence may signal at any time; its
    // block only goes back on the idle list through the retire walk, so
    // don't run one until frame B has taken its block.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close frame A");
    assert_eq!(
        idle(&engine),
        0,
        "a closed frame must not return its blocks before retiring"
    );

    // Frame B, opened while A is in flight: a block of its own.
    let before = thread_alloc_calls();
    draw(&mut engine, &mut store, &mut platform);
    assert_eq!(
        thread_alloc_calls() - before,
        1,
        "frame B allocates a block"
    );
    let b = open_slices(&engine);
    assert_eq!(b.len(), 1);
    assert_ne!(
        b[0].buffer, a[0].buffer,
        "frame B was handed frame A's block while A was in flight"
    );

    // Both frames submitted and complete: both blocks go idle.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close frame B");
    settle(&mut engine, &mut store, &mut platform);
    assert_eq!(idle(&engine), 2);

    // Frame C reuses one of them without allocating.
    let before = thread_alloc_calls();
    draw(&mut engine, &mut store, &mut platform);
    assert_eq!(thread_alloc_calls() - before, 0, "reused, not allocated");
    let c = open_slices(&engine);
    assert!(c[0].buffer == a[0].buffer || c[0].buffer == b[0].buffer);
    assert_eq!(c[0].offset, 0);
    assert_eq!(idle(&engine), 1);

    engine.drain_all(&mut platform);
}

/// #177: requests in one frame are bump-allocated from one block at
/// offsets aligned for their use, and their bytes land at those
/// offsets; a request larger than a block gets a dedicated buffer.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn upload_to_frame_suballocates_aligned_slices_and_falls_back_for_oversize() {
    use super::super::upload_arena::{BLOCK_BYTES, Placement};
    use crate::kms::vk::mem_accounting::{ChurnClass, thread_alloc_calls};
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let ticket = platform.submit_group_ticket_or_open().expect("ticket");
    let inner = engine.inner.as_mut().expect("inner");
    inner.frame_builder.open_for_paint(ticket, 1);

    let copy_align = inner.upload_copy_align;
    assert!(
        copy_align.is_power_of_two() && copy_align >= 4,
        "{copy_align}"
    );
    assert!(copy_align <= UPLOAD_COPY_ALIGN_MAX);

    let before = thread_alloc_calls();
    let v1: Vec<u8> = (0..36).collect();
    let g: Vec<u8> = vec![0xA5; 3];
    let v2: Vec<u8> = (100..140).collect();
    let p1 = inner
        .upload_to_frame(&v1, UPLOAD_VERTEX_ALIGN, ChurnClass::GlyphRun)
        .expect("v1");
    let pg = inner
        .upload_to_frame(&g, copy_align, ChurnClass::GlyphUpload)
        .expect("g");
    let p2 = inner
        .upload_to_frame(&v2, UPLOAD_VERTEX_ALIGN, ChurnClass::Traps)
        .expect("v2");
    assert_eq!(
        thread_alloc_calls() - before,
        1,
        "three requests, one block"
    );

    let open = inner.frame_builder.open.as_ref().expect("open");
    let slice =
        |i: super::super::frame_builder::PinnedUploadIdx| open.pins.upload_slices[i.0 as usize];
    let (s1, sg, s2) = (slice(p1), slice(pg), slice(p2));
    assert_eq!(s1.buffer, sg.buffer);
    assert_eq!(s1.buffer, s2.buffer);
    assert_eq!(s1.offset, 0);
    assert_eq!(sg.offset % copy_align, 0);
    assert!(sg.offset >= 36);
    assert_eq!(s2.offset % UPLOAD_VERTEX_ALIGN, 0);
    assert!(s2.offset >= sg.offset + 3);
    assert_eq!(open.pins.uploads.shared_len(), 1);
    // The bytes are where the slices say.
    let block = open.pins.uploads.block(Placement::Shared(0));
    assert_eq!(block.buffer, s1.buffer);
    assert_eq!(block.size, BLOCK_BYTES);
    for (off, want) in [(s1.offset, &v1), (sg.offset, &g), (s2.offset, &v2)] {
        let off = usize::try_from(off).expect("offset");
        // SAFETY: the block is mapped for BLOCK_BYTES and the slice lies
        // inside it; nothing else writes it.
        let got = unsafe { std::slice::from_raw_parts(block.mapped.as_ptr().add(off), want.len()) };
        assert_eq!(got, want.as_slice());
    }

    // Oversize: a dedicated buffer at offset 0, the shared head untouched.
    let big = vec![7u8; usize::try_from(BLOCK_BYTES).expect("size") + 1];
    let before = thread_alloc_calls();
    let pb = inner
        .upload_to_frame(&big, UPLOAD_VERTEX_ALIGN, ChurnClass::Traps)
        .expect("big");
    let p3 = inner
        .upload_to_frame(&v1, UPLOAD_VERTEX_ALIGN, ChurnClass::GlyphRun)
        .expect("v3");
    assert_eq!(
        thread_alloc_calls() - before,
        1,
        "only the dedicated buffer"
    );
    let open = inner.frame_builder.open.as_ref().expect("open");
    let sb = open.pins.upload_slices[pb.0 as usize];
    let s3 = open.pins.upload_slices[p3.0 as usize];
    assert_ne!(sb.buffer, s1.buffer);
    assert_eq!(sb.offset, 0);
    assert_eq!(open.pins.uploads.dedicated_len(), 1);
    assert_eq!(s3.buffer, s1.buffer, "the shared block keeps filling");
    assert!(s3.offset > s2.offset);
    assert_eq!(open.pins.len(), 5, "one pin per request");

    engine
        .close_open_frame_for_timeout_for_tests(&mut DrawableStore::new(), &mut platform)
        .expect("close");
    engine.drain_all(&mut platform);
}

/// `image_text`'s identical hole (`render/engine.rs:5954`'s
/// per-glyph admission rule, same unconditional instance pin
/// afterwards). Same shape as the `composite_glyphs` case above,
/// through the core-font path instead of the glyphset path.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn image_text_pin_ceiling_reserves_instance_buffer_pin() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a_with_kind(
        &platform,
        &mut store,
        0x1,
        32,
        32,
        super::super::store::DrawableKind::Window,
        true,
    );

    {
        let inner = engine.inner.as_mut().expect("inner");
        inner.frame_builder.set_max_pinned_resources_per_frame(2);
    }

    let glyphs = vec![
        build_glyph(u32::from(b'A'), 1, 1, 2, 2),
        build_glyph(u32::from(b'B'), 10, 1, 2, 2),
    ];

    let stats = engine
        .image_text(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            7,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
        )
        .expect("image_text");

    let inner = engine.inner.as_ref().expect("inner");
    let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame stays open after image_text");
    assert!(
        open.pins.len() <= ceiling,
        "pin set must never exceed the declared ceiling: {} pins against a \
             ceiling of {} (glyphs_dropped={})",
        open.pins.len(),
        ceiling,
        stats.glyphs_dropped,
    );

    engine.drain_all(&mut platform);
}

// ── #137 step 4a: `first_instance`, plumbed end to end ──────
//
// A `CompositeGlyphs` request will have to be recorded as SEVERAL
// contiguous draw runs — glyphs of different `GlyphLayout`s need
// different pipelines, and pipeline state is immutable — all
// sharing ONE instance buffer, so the request still costs exactly
// one frame pin (the one the ceiling reserves). Each run therefore
// carries a `(first_instance, instance_count)` range.
//
// Production forms exactly one run today, so `record_glyph_runs`
// is the seam these two tests drive. The first drives the SAME
// function production calls, handed two ranges instead of one; the
// second pins `cmd_draw`'s `firstInstance` argument at its own
// level, without the recorder in between.
//
// The failure mode is an unplumbed `first_instance`: a run whose
// offset stays 0 draws run 0's glyphs a second time instead of its
// own. Nothing downstream of a run splitter could tell that apart
// from a splitter bug, which is why it is pinned here, before the
// splitter exists.

/// The glyphset the step-4a fixtures intern into: two 2×2 glyphs,
/// each of HALF coverage. Half, and composited with `Add`, so
/// drawing one of them twice is observable — see
/// `RUN_SPLIT_OP`.
const RUN_SPLIT_GS: u32 = 0x7401;
/// `Add` (wire PictOp 12), the op the run-split fixture composites
/// with. It is deliberately NOT idempotent at partial coverage:
/// under `Over` with opaque foreground, drawing range 0's glyph a
/// second time lands the same white on the same pixels, so a
/// `first_instance` error that ALSO widens the count (`count =
/// end - first_instance`, which is how the recorder computes it)
/// would paint an indistinguishable result. With `Add` at coverage
/// 0x80 the double draw saturates to 0xFF and the two
/// destinations differ.
const RUN_SPLIT_OP: u8 = 12;
/// Half coverage, so `Add`ing it twice is distinguishable from
/// `Add`ing it once.
const RUN_SPLIT_COVERAGE: u8 = 0x80;
/// Left glyph: instance 0 of the shared buffer.
const RUN_SPLIT_DST_0: (i32, i32) = (2, 2);
/// Right glyph: instance 1. Disjoint from instance 0's quad, so
/// "instance 1 was never drawn" is directly observable.
const RUN_SPLIT_DST_1: (i32, i32) = (10, 2);

fn run_split_glyph_inputs(pixels: &[u8; 4]) -> [CompositeGlyphInput<'_>; 2] {
    [
        CompositeGlyphInput {
            gs_xid: RUN_SPLIT_GS,
            glyph_id: 1,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(pixels),
            dst_x: RUN_SPLIT_DST_0.0,
            dst_y: RUN_SPLIT_DST_0.1,
        },
        CompositeGlyphInput {
            gs_xid: RUN_SPLIT_GS,
            glyph_id: 2,
            w: 2,
            h: 2,
            pixels: GlyphPixels::A8(pixels),
            dst_x: RUN_SPLIT_DST_1.0,
            dst_y: RUN_SPLIT_DST_1.1,
        },
    ]
}

/// The two interned glyphs as `RecordedTextGlyph`s at the fixture
/// positions — the same values the per-glyph walk in
/// `composite_glyphs_via_frame_builder` builds.
fn run_split_recorded_glyphs(
    engine: &RenderEngine,
) -> Vec<super::super::frame_builder::RecordedTextGlyph> {
    let atlas = engine
        .inner
        .as_ref()
        .expect("inner")
        .glyph_atlas
        .as_ref()
        .expect("atlas built by the production call");
    [(1_u32, RUN_SPLIT_DST_0), (2_u32, RUN_SPLIT_DST_1)]
        .into_iter()
        .map(|(glyph_id, (dst_x, dst_y))| {
            let entry = atlas
                .lookup(GlyphKey {
                    font_xid: RUN_SPLIT_GS,
                    codepoint: glyph_id,
                })
                .expect("glyph committed to the atlas by the drained frame");
            super::super::frame_builder::RecordedTextGlyph {
                atlas_x: entry.atlas_x,
                atlas_y: entry.atlas_y,
                logical_w: entry.logical_w,
                h: entry.h,
                dst_x,
                dst_y,
                layout: entry.layout,
            }
        })
        .collect()
}

/// Two A8 glyphs recorded as **two** `(first_instance, count)`
/// ranges through `record_glyph_runs` must render exactly what the
/// same two glyphs render as production's single range.
///
/// With `first_instance` unplumbed, the second range redraws the
/// first glyph and the right-hand quad never appears, so the two
/// destinations differ. The two "must be painted" assertions keep
/// the comparison from passing on two blank images.
/// #177: a glyph run's instance data lives in an upload arena block
/// that is allocated and freed within one telemetry period here
/// (`drain_all` empties the idle list), so the live ledger (`vram by
/// use`) never sees it. The churn counters must: `upload_arena` gains a
/// block allocation and free, the arena counts a sub-allocation sized
/// for the run, and the formatted line shows non-zero rates. Other
/// tests run in parallel against the same process-wide counters, so
/// this asserts lower bounds only.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_glyph_run_upload_shows_in_churn_rates() {
    use crate::kms::vk::mem_accounting::{ChurnClass, churn_snapshot, format_churn_line};
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let pixels = [0xFFu8; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    let draw =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .composite_glyphs(
                    store,
                    platform,
                    Dst::server_internal(target),
                    3,
                    0,
                    [1.0, 1.0, 1.0, 1.0],
                    &glyphs,
                    None,
                )
                .expect("composite_glyphs");
            // Closes the frame; `drain_all` then retires it and frees its
            // upload block.
            engine
                .get_image(store, platform, Src::server_internal(target), full, 32)
                .expect("get_image");
            engine.drain_all(platform);
        };
    // Warm-up interns the glyphs so the measured run allocates only
    // what every later run of the same text allocates.
    draw(&mut engine, &mut store, &mut platform);

    let before = churn_snapshot();
    draw(&mut engine, &mut store, &mut platform);
    let after = churn_snapshot();

    let (b, a) = (
        before.class(ChurnClass::UploadArena),
        after.class(ChurnClass::UploadArena),
    );
    let instance = std::mem::size_of::<crate::kms::vk::text_pipeline::GlyphInstanceData>();
    assert!(a.allocs > b.allocs, "upload block allocation not counted");
    assert!(a.frees > b.frees, "upload block free not counted");
    let (br, ar) = (before.upload_arena, after.upload_arena);
    assert!(
        ar.suballocs > br.suballocs,
        "glyph run sub-allocation not counted"
    );
    assert!(
        ar.suballoc_bytes - br.suballoc_bytes >= 2 * instance as u64,
        "glyph run bytes: {} < 2 instances",
        ar.suballoc_bytes - br.suballoc_bytes
    );
    assert!(
        after.class(ChurnClass::Readback).frees > before.class(ChurnClass::Readback).frees,
        "get_image readback staging not counted"
    );
    let line = format_churn_line(&before, &after, 1.0, None);
    let seg = |name: &str| {
        line.split(&format!(" {name}["))
            .nth(1)
            .and_then(|s| s.split(']').next())
            .unwrap_or_else(|| panic!("no {name} segment: {line}"))
            .to_owned()
    };
    let blocks = seg("upload_arena");
    assert!(
        !blocks.starts_with("alloc=0/s") && !blocks.contains(" free=0/s"),
        "rate line misses the block churn: {line}"
    );
    assert!(
        !seg("arena").contains(" sub=0/s"),
        "rate line misses the sub-allocation: {line}"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_glyph_run_split_into_two_ranges_renders_as_one_range() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let one_range = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let two_ranges = alloc_drawable_3a(&platform, &mut store, 0x2, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let black = [0.0, 0.0, 0.0, 1.0];
    let white = [1.0, 1.0, 1.0, 1.0];

    // Both destinations start from the same fully-defined content
    // (the storage allocation itself is not initialised — see
    // `window_storage_init_covers_the_whole_allocation`).
    for id in [one_range, two_ranges] {
        engine
            .fill_rect(
                &mut store,
                &mut platform,
                Dst::server_internal(id),
                full,
                black,
            )
            .expect("fill_rect");
    }

    // (1) The baseline, through production: ONE run over both
    //     glyphs. This also interns them and builds the
    //     (Over, BGRA8, has-alpha) text pipeline that step (3)
    //     reuses.
    let pixels = [RUN_SPLIT_COVERAGE; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(one_range),
            RUN_SPLIT_OP,
            0, // pict_format unknown → depth heuristic
            white,
            &glyphs,
            None,
        )
        .expect("composite_glyphs");
    // Read the baseline back. `get_image` is what CLOSES the open
    // frame (`drain_all` holds no `store` borrow and so cannot
    // commit a close), and the atlas cache inserts are
    // transactional on close-success — so this is also what makes
    // the entries below look-up-able.
    let out_one = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(one_range),
            full,
            32,
        )
        .expect("get_image one range");

    // (2) Reopen a frame on the second destination and first-touch
    //     it, exactly as any second op in a frame would find it.
    //     Its content is already the black fill from above; this
    //     repeats it only to open a frame and touch the drawable.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(two_ranges),
            full,
            black,
        )
        .expect("fill_rect reopen");

    // (3) The same glyphs, recorded as TWO ranges over one shared
    //     instance buffer, through the function production uses.
    let recorded = run_split_recorded_glyphs(&engine);
    assert_eq!(recorded.len(), 2, "both glyphs must have interned");
    let pins_before = engine
        .inner
        .as_ref()
        .expect("inner")
        .frame_builder
        .open
        .as_ref()
        .expect("frame open after fill_rect")
        .pins
        .len();
    let inner = engine.inner.as_mut().expect("inner");
    let dst_old_layout = inner.current_layout_for_drawable(&store, two_ranges);
    let instances = RenderEngine::record_glyph_runs(
        inner,
        &[&recorded[..1], &recorded[1..]],
        &GlyphRunCommon {
            dst_id: two_ranges,
            dst_old_layout,
            op: RUN_SPLIT_OP,
            dst_has_alpha: dst_has_alpha_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
            foreground_rgba: white,
            clip_scissors: vec![full],
        },
    )
    .expect("record_glyph_runs");
    assert_eq!(instances, 2, "both glyphs must have become instances");
    store.mark_contents_modified(two_ranges);

    // Two runs, ONE instance pin — the property the frame-pin
    // ceiling's single reserved pin rests on.
    {
        let open = engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .open
            .as_ref()
            .expect("frame still open");
        assert_eq!(
            open.pins.len(),
            pins_before + 1,
            "two runs must share ONE pinned instance buffer",
        );
        let runs = open
            .ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    super::super::frame_builder::RecordedOp::CompositeGlyphs(_)
                )
            })
            .count();
        assert_eq!(runs, 2, "the helper must have recorded two runs");
    }

    // (4) Read the two-range destination back and compare.
    let out_two = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(two_ranges),
            full,
            32,
        )
        .expect("get_image two ranges");

    // Teeth: both quads really are painted, so the equality below
    // is not comparing two black images.
    let px = |buf: &[u8], x: usize, y: usize| {
        let o = (y * 32 + x) * 4;
        [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
    };
    let bg = px(&out_one, 30, 30);
    for (x, y) in [
        (RUN_SPLIT_DST_0.0 as usize, RUN_SPLIT_DST_0.1 as usize),
        (RUN_SPLIT_DST_1.0 as usize, RUN_SPLIT_DST_1.1 as usize),
    ] {
        assert_ne!(
            px(&out_one, x, y),
            bg,
            "the one-range baseline must paint the glyph at ({x}, {y})",
        );
        assert_ne!(
            px(&out_two, x, y),
            bg,
            "the two-range recording must paint the glyph at ({x}, {y}) — a \
                 `first_instance` left at zero redraws range 0's glyph instead",
        );
    }
    assert_eq!(
        out_two, out_one,
        "recording the same glyphs as two ranges over one shared instance \
             buffer must be pixel-identical to recording them as one range",
    );

    engine.drain_all(&mut platform);
}

/// `record_text_run_scissored` at a NONZERO `first_instance`, with
/// no recorder in between: a 2-instance buffer drawn as the range
/// `[1, 2)` must paint the second glyph's quad and leave the
/// first's untouched.
///
/// This is the argument-level companion to the seam test above —
/// it fails if `first_instance` reaches the function but not
/// `cmd_draw`'s fourth argument.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn record_text_run_scissored_draws_only_the_requested_instance_range() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let Some(pool) = platform.ops_command_pool_handle() else {
        eprintln!("no ops command pool — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let white = [1.0, 1.0, 1.0, 1.0];

    // Intern the glyphs + build the text pipeline through
    // production, then drain so the atlas cache inserts commit and
    // the atlas image is left in SHADER_READ_ONLY_OPTIMAL.
    let pixels = [0xFFu8; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect");
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            3,
            0,
            white,
            &glyphs,
            None,
        )
        .expect("composite_glyphs warm-up");
    // `get_image` is what closes the frame, and the atlas cache
    // inserts commit on close-success — `drain_all` holds no
    // `store` borrow and cannot close.
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image closes the warm-up frame");

    // Repaint the destination black so the warm-up's own quads are
    // gone and only this test's draw can put pixels down — and
    // close again, so no recorded op can land AFTER the
    // out-of-band draw below and overwrite it.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect clear");
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image closes the clear frame");

    // A 2-instance buffer, in glyph order.
    let recorded = run_split_recorded_glyphs(&engine);
    let mut bytes: Vec<u8> = Vec::new();
    for g in &recorded {
        let inst = crate::kms::vk::text_pipeline::GlyphInstanceData::from_glyph(
            g.dst_x,
            g.dst_y,
            g.atlas_x,
            g.atlas_y,
            g.logical_w,
            g.h,
            g.layout,
        )
        .expect("instance geometry");
        bytes.extend_from_slice(inst.as_bytes());
    }
    {
        let inner = engine.inner.as_mut().expect("inner");
        let vk_ctx = Arc::clone(&inner.vk);
        let buf = StagingBuffer::new_with_usage(
            Arc::clone(&vk_ctx),
            u64::try_from(bytes.len()).expect("len"),
            vk::BufferUsageFlags::VERTEX_BUFFER,
            crate::kms::vk::mem_accounting::ChurnClass::GlyphRun,
        )
        .expect("instance buffer");
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.mapped.as_ptr(), bytes.len());
        }
        let atlas_extent = inner.glyph_atlas.as_ref().expect("atlas").extent();
        let pipeline = inner
            .text_pipelines
            .get(&(3, vk::Format::B8G8R8A8_UNORM, true, false))
            .expect("pipeline built by the warm-up");
        let drawable = store.get_mut(target).expect("target");
        let mut adapter = StorageTextTarget {
            extent: drawable.storage.extent,
            image: drawable.storage.image,
            image_view: drawable.storage.image_view,
            current_layout: drawable.storage.current_layout,
        };
        crate::kms::vk::ops::run_one_shot_op(&vk_ctx, pool, |vk, cb| {
            crate::kms::vk::ops::text::record_text_run_scissored(
                vk,
                cb,
                &mut adapter,
                atlas_extent,
                pipeline,
                buf.buffer,
                0,
                // Range [1, 2): the SECOND instance only.
                1,
                1,
                white,
                &[full],
            )
        })
        .expect("one-shot text run");
        drawable.storage.current_layout = adapter.current_layout;
    }

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(target),
            full,
            32,
        )
        .expect("get_image");
    let px = |x: usize, y: usize| {
        let o = (y * 32 + x) * 4;
        [out[o], out[o + 1], out[o + 2], out[o + 3]]
    };
    let bg = px(30, 30);
    assert_ne!(
        px(RUN_SPLIT_DST_1.0 as usize, RUN_SPLIT_DST_1.1 as usize),
        bg,
        "instance 1 is the range's only member and must be drawn",
    );
    assert_eq!(
        px(RUN_SPLIT_DST_0.0 as usize, RUN_SPLIT_DST_0.1 as usize),
        bg,
        "instance 0 is BELOW first_instance and must not be drawn — a \
             hardcoded firstInstance of 0 draws it",
    );

    engine.drain_all(&mut platform);
}

// ── #137 step 4b: the run splitter ────────────────────────────
//
// A single `CompositeGlyphs` request can switch glyphset
// mid-stream (the inline `count == 255` items element) and
// glyphsets can differ in picture format, so one request can
// interleave A8 and ARGB32 glyphs — which need different
// pipelines, and pipeline state is immutable. The request is
// therefore recorded as several contiguous runs.
//
// Both tests below share one fixture: a real items stream over
// two glyphsets of different formats, parsed by the production
// parse (`parse_composite_glyph_items`). The first asserts the
// splitter's ORDER; the second asserts that today it produces
// exactly one run.

/// Two glyphsets — A8 and ARGB32 — and an items stream that
/// alternates between them via the inline `count == 255`
/// element. Returns `(glyphsets, initial_gs_xid, items)`.
///
/// The stream is deliberately not one-glyph-per-element:
/// elements of 2, 1, 1 and 2 glyphs, so a run has to span an
/// element boundary (glyphs 0-1) and two same-format glyphs
/// inside one element must stay in one run (glyphs 4-5). Source
/// formats come out as A8, A8, ARGB32, A8, ARGB32, ARGB32 and
/// the pen advances one pixel per glyph, so `dst_x` doubles as
/// each glyph's index in request order.
fn mixed_format_items_fixture() -> (HashMap<u32, crate::kms::core::GlyphSetState>, u32, Vec<u8>) {
    use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

    const GS_A8: u32 = 0x4B01;
    const GS_ARGB32: u32 = 0x4B02;

    let mut glyphsets: HashMap<u32, GlyphSetState> = HashMap::new();
    for (xid, format, bytes_per_pixel) in [
        (GS_A8, GlyphSetFormat::A8, 1),
        (GS_ARGB32, GlyphSetFormat::Argb32, 4),
    ] {
        let mut glyphs = HashMap::new();
        // Ids 1..=6, so every glyph the stream names resolves in
        // whichever glyphset is active at the time.
        for glyph_id in 1..=6u32 {
            glyphs.insert(
                glyph_id,
                StoredGlyph {
                    width: 1,
                    height: 1,
                    x: 0,
                    y: 0,
                    x_off: 1,
                    y_off: 0,
                    pixels: vec![0x80; bytes_per_pixel],
                    format,
                },
            );
        }
        glyphsets.insert(xid, GlyphSetState { format, glyphs });
    }

    // Element layout: count(u8) pad pad pad dx(i16) dy(i16), then
    // `count` 1-byte ids padded to a 4-byte boundary (minor 23).
    let mut items: Vec<u8> = Vec::new();
    let element = |items: &mut Vec<u8>, ids: &[u8]| {
        items.extend_from_slice(&[u8::try_from(ids.len()).expect("count"), 0, 0, 0, 0, 0, 0, 0]);
        items.extend_from_slice(ids);
        while !items.len().is_multiple_of(4) {
            items.push(0);
        }
    };
    let switch_to = |items: &mut Vec<u8>, xid: u32| {
        items.extend_from_slice(&[255u8, 0, 0, 0]);
        items.extend_from_slice(&xid.to_le_bytes());
    };
    element(&mut items, &[1, 2]); // glyphs 0,1 — A8 (initial gs)
    switch_to(&mut items, GS_ARGB32);
    element(&mut items, &[3]); // glyph 2 — ARGB32
    switch_to(&mut items, GS_A8);
    element(&mut items, &[4]); // glyph 3 — A8
    switch_to(&mut items, GS_ARGB32);
    element(&mut items, &[5, 6]); // glyphs 4,5 — ARGB32

    (glyphsets, GS_A8, items)
}

/// The parsed glyphs as the per-glyph walk would record them —
/// one `RecordedTextGlyph` per parsed glyph, same order, each
/// tagged with the effective layout a device with (or without)
/// `dualSrcBlend` gives it. The atlas coordinates are irrelevant
/// to the splitter; `dst_x` carries the glyph's request-order
/// index.
fn recorded_from_parsed(
    parsed: &[super::super::backend::ParsedGlyph],
    component_alpha_supported: bool,
) -> Vec<super::super::frame_builder::RecordedTextGlyph> {
    parsed
        .iter()
        .map(|p| super::super::frame_builder::RecordedTextGlyph {
            atlas_x: 0,
            atlas_y: 0,
            logical_w: p.w,
            h: p.h,
            dst_x: p.dst_x,
            dst_y: p.dst_y,
            layout: RenderEngine::effective_glyph_layout(
                p.source_format,
                component_alpha_supported,
            ),
        })
        .collect()
}

/// The splitter must cut CONTIGUOUS runs in REQUEST ORDER.
///
/// Homogeneity alone is not the property worth testing: a
/// splitter that gathers all the A8 glyphs into one run and all
/// the component-alpha glyphs into another is perfectly
/// homogeneous and wrong — PictOps are not commutative, so
/// reordering changes pixels wherever two glyph quads overlap,
/// and overlap is ordinary (kerning, italics, combining marks).
/// So the load-bearing assertion is that concatenating the runs
/// reproduces the input glyph sequence exactly: same glyphs, same
/// order, none lost or duplicated at a boundary.
///
/// The layout sequence here is heterogeneous, which is what
/// `effective_glyph_layout` answers for this stream on a device
/// WITH `dualSrcBlend` — i.e. what production produces on
/// lavapipe, RADV and every desktop GPU. The companion test
/// below covers the device that has none, where the same stream
/// collapses to one run.
#[test]
fn a_mixed_format_items_stream_splits_into_ordered_homogeneous_runs() {
    let (glyphsets, initial_gs, items) = mixed_format_items_fixture();
    let parsed = super::super::backend::parse_composite_glyph_items(
        &glyphsets, 23, initial_gs, 0, 0, &items,
    );

    // The tag follows the inline glyphset change, in request
    // order. If the parse ignored the `count == 255` element this
    // would be all-A8, and no splitter could recover.
    assert_eq!(
        parsed
            .glyphs
            .iter()
            .map(|p| p.source_format)
            .collect::<Vec<_>>(),
        vec![
            GlyphSourceFormat::A8,
            GlyphSourceFormat::A8,
            GlyphSourceFormat::Argb32,
            GlyphSourceFormat::A8,
            GlyphSourceFormat::Argb32,
            GlyphSourceFormat::Argb32,
        ],
        "source-format tags must follow the inline glyphset change, in request order",
    );

    // Layouts as a `dualSrcBlend` device gives them: ARGB32
    // interns as four packed planes, A8 as one.
    let recorded = recorded_from_parsed(&parsed.glyphs, true);
    // Request-order index, carried in dst_x by the fixture's
    // one-pixel pen advance.
    assert_eq!(
        recorded.iter().map(|g| g.dst_x).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5],
        "the fixture's pen must advance one pixel per glyph",
    );

    let layouts: Vec<GlyphLayout> = recorded.iter().map(|g| g.layout).collect();
    assert_eq!(
        layouts,
        vec![
            GlyphLayout::A8,
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
            GlyphLayout::ComponentAlpha,
        ],
        "on a dualSrcBlend device ARGB32 interns as four packed planes",
    );
    let runs = RenderEngine::split_glyph_runs(&recorded);

    // (a) Contiguous and maximal: 2 + 1 + 1 + 2. One run per
    //     glyph would also be homogeneous and ordered; these
    //     lengths reject it, and they prove a run spans an
    //     element boundary (glyphs 0-1) while two same-format
    //     glyphs in one element stay together (glyphs 4-5).
    assert_eq!(
        runs.iter().map(|r| r.len()).collect::<Vec<_>>(),
        vec![2, 1, 1, 2],
        "runs must be maximal contiguous stretches of one layout",
    );

    // (b) ORDER, and nothing lost or duplicated: the runs
    //     concatenated ARE the input sequence.
    let flattened: Vec<super::super::frame_builder::RecordedTextGlyph> =
        runs.iter().flat_map(|r| r.iter().copied()).collect();
    assert_eq!(
        flattened, recorded,
        "concatenating the runs must reproduce the glyphs in request order",
    );

    // (c) Homogeneous, and adjacent runs differ — so the cut is
    //     exactly where the layout changes.
    let mut at = 0usize;
    let mut run_layouts = Vec::new();
    for run in &runs {
        let slice = &layouts[at..at + run.len()];
        assert!(
            slice.iter().all(|l| *l == slice[0]),
            "run at {at} mixes layouts: {slice:?}",
        );
        run_layouts.push(slice[0]);
        at += run.len();
    }
    assert_eq!(at, layouts.len(), "the runs must cover every glyph");
    assert_eq!(
        run_layouts,
        vec![
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
            GlyphLayout::A8,
            GlyphLayout::ComponentAlpha,
        ],
        "adjacent runs must differ in layout, in request order",
    );
}

/// The splitter is inert **exactly where `dualSrcBlend` is
/// absent**, and only there.
///
/// This test used to assert inertness unconditionally, because
/// step 3's upload reduction applied on every device: an ARGB32
/// glyph WAS an A8 glyph in the atlas, so a mixed request could
/// only ever form one run. Step 5 narrowed that reduction to
/// `!component_alpha_supported`, and lavapipe and RADV both
/// report `dualSrcBlend`, so the unconditional claim is now
/// false on every device CI and the desktop actually run on.
///
/// What survives is the conditional half, which is worth more:
/// with the reduction in force every glyph of a mixed stream has
/// the SAME effective layout, so the split really is inert there,
/// and a device that cannot blend four planes never records an op
/// asking it to. The `true` case — several runs from the same
/// stream — is the sibling test above.
#[test]
fn the_run_split_is_inert_only_where_component_alpha_is_unsupported() {
    let (glyphsets, initial_gs, items) = mixed_format_items_fixture();
    let parsed = super::super::backend::parse_composite_glyph_items(
        &glyphsets, 23, initial_gs, 0, 0, &items,
    );
    assert_eq!(parsed.glyphs.len(), 6, "fixture must parse six glyphs");
    assert!(
        parsed
            .glyphs
            .iter()
            .any(|p| p.source_format == GlyphSourceFormat::Argb32),
        "fixture must actually mix formats, or inertness is vacuous",
    );

    // No `dualSrcBlend`: the upload reduces ARGB32 to one
    // grayscale coverage plane, so every glyph is an A8 entry.
    let recorded = recorded_from_parsed(&parsed.glyphs, false);
    let layouts: Vec<GlyphLayout> = recorded.iter().map(|g| g.layout).collect();
    assert!(
        layouts.iter().all(|l| *l == GlyphLayout::A8),
        "without dualSrcBlend the upload reduces every source format to one \
             A8 plane: {layouts:?}",
    );

    let runs = RenderEngine::split_glyph_runs(&recorded);
    assert_eq!(runs.len(), 1, "a reduced request must record as ONE run");
    assert_eq!(
        runs[0],
        &recorded[..],
        "the single run must carry every glyph, in request order",
    );

    // And the same stream on a device that CAN blend four planes
    // does not collapse — so the assertion above is a statement
    // about the device, not a tautology about the fixture.
    let with_ca = recorded_from_parsed(&parsed.glyphs, true);
    assert_eq!(
        RenderEngine::split_glyph_runs(&with_ca).len(),
        4,
        "with dualSrcBlend the same mixed stream must record as four runs",
    );
}

/// The layout derivation itself, as a table — the one place
/// `component_alpha_supported` is consulted for glyphs, and the
/// pure test of the `dualSrcBlend`-less fallback's SELECTION
/// (`glyph_pixels` tests pin the reduction's arithmetic).
///
/// No runtime switch is needed to reach either column: the
/// function takes the capability as an argument
/// (`feedback_no_feature_kill_switches`).
#[test]
fn the_effective_layout_answers_component_alpha_only_for_argb32_on_a_capable_device() {
    for supported in [false, true] {
        for source in [GlyphSourceFormat::A8, GlyphSourceFormat::A1] {
            assert_eq!(
                RenderEngine::effective_glyph_layout(source, supported),
                GlyphLayout::A8,
                "{source:?} is a single coverage plane whatever the device does",
            );
        }
    }
    assert_eq!(
        RenderEngine::effective_glyph_layout(GlyphSourceFormat::Argb32, true),
        GlyphLayout::ComponentAlpha,
        "ARGB32 on a dualSrcBlend device packs four planes",
    );
    assert_eq!(
        RenderEngine::effective_glyph_layout(GlyphSourceFormat::Argb32, false),
        GlyphLayout::A8,
        "ARGB32 without dualSrcBlend reduces to one grayscale plane — the \
             fallback vk/device.rs already promises",
    );
}

// ── #137 step 5: the packed footprint reaches the UPLOAD ─────
//
// `AtlasEntry` carries `packed_w` (what the packer reserved and
// what the copy region covers) and `logical_w` (the glyph's own
// size). Step 2 split them but could not assert which one the
// recorded upload gets, because production built both from a
// single variable and no call path produced an asymmetric entry.
// Component alpha is the first and only path where they differ,
// so this is the first step where the assertion is reachable —
// and the failure mode is an upload copying a QUARTER of the
// glyph, three planes left as whatever the atlas held.

/// The recorded `GlyphUpload` for a component-alpha glyph carries
/// the PACKED width, and its committed entry carries both widths.
///
/// The op is inspected on the still-open frame, before the close
/// that replays it — `packed_w` is exactly what
/// `GlyphAtlas::record_upload` passes as the copy region's
/// `image_extent.width`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_component_alpha_glyph_upload_records_the_packed_width() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);

    // 2x2 ARGB32 wire, dense CARD32 rows: [B, G, R, A] per pixel.
    let wire: [u8; 16] = [
        0x10, 0x20, 0x30, 0x40, 0x11, 0x21, 0x31, 0x41, 0x12, 0x22, 0x32, 0x42, 0x13, 0x23, 0x33,
        0x43,
    ];
    let glyphs = [CompositeGlyphInput {
        gs_xid: 0x7501,
        glyph_id: 1,
        w: 2,
        h: 2,
        pixels: GlyphPixels::Argb32Wire(&wire),
        dst_x: 1,
        dst_y: 1,
    }];
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(target),
            3, // Over
            0,
            [1.0, 1.0, 1.0, 1.0],
            &glyphs,
            None,
        )
        .expect("composite_glyphs");

    let supported = engine
        .inner
        .as_ref()
        .expect("inner")
        .vk
        .component_alpha_supported;
    let uploads: Vec<(u32, u32, GlyphLayout, u32)> = engine
        .inner
        .as_ref()
        .expect("inner")
        .frame_builder
        .open
        .as_ref()
        .expect("the call opened a frame")
        .ops
        .iter()
        .filter_map(|op| match op {
            super::super::frame_builder::RecordedOp::GlyphUpload(up) => Some((
                up.packed_w,
                up.h,
                up.insert_entry.layout,
                up.insert_entry.logical_w,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(uploads.len(), 1, "one glyph, one recorded upload");
    let (packed_w, h, layout, logical_w) = uploads[0];
    assert_eq!(h, 2);
    assert_eq!(logical_w, 2, "the entry's logical width is the glyph's own");

    if supported {
        assert_eq!(
            layout,
            GlyphLayout::ComponentAlpha,
            "a dualSrcBlend device must intern ARGB32 as four planes",
        );
        assert_eq!(
            packed_w, 8,
            "the recorded upload must copy the PACKED footprint 4 * 2 = 8; \
                 receiving the logical width instead copies a quarter of the \
                 glyph and leaves three planes as whatever the atlas held",
        );
        assert_ne!(
            packed_w, logical_w,
            "this is the one path where the two widths differ — if they are \
                 equal here the assertion above is vacuous",
        );
    } else {
        // No dualSrcBlend: the upload reduced to one grayscale
        // plane, so the two widths coincide and there is nothing
        // asymmetric to catch here.
        assert_eq!(layout, GlyphLayout::A8);
        assert_eq!(packed_w, logical_w);
    }

    engine.drain_all(&mut platform);
}

/// #137 step 4b, carry-forward from 4a: the destination layout on
/// runs 2+.
///
/// Run 0 finds the destination in the frame's pre-op layout;
/// every later run finds it as the previous run left it, and
/// `record_text_run_scissored` ends in `SHADER_READ_ONLY_OPTIMAL`.
/// Carrying the pre-op layout on run 2+ declares a wrong
/// `oldLayout` in its barrier — and with a pre-op `UNDEFINED`,
/// which is exactly what a first-touched destination has, the
/// driver is then licensed to DISCARD the earlier runs' pixels.
///
/// The oracle is the RECORDED value, not pixels: an `UNDEFINED`
/// `oldLayout` is *permitted* to preserve contents, so a pixel
/// assertion on lavapipe passes whether the barrier is right or
/// wrong. Asserting `dst_old_layout` per run is exact and
/// driver-independent. The closing `get_image` then confirms the
/// recorded barriers actually emit and submit.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_second_glyph_run_records_the_layout_the_first_run_left() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let warm = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    // The split destination is never painted before the runs are
    // recorded, so its pre-op layout is the dangerous one.
    let split_dst = alloc_drawable_3a(&platform, &mut store, 0x2, 32, 32);
    let full = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 32,
            height: 32,
        },
    };
    let white = [1.0, 1.0, 1.0, 1.0];

    // Warm-up through production: interns the two fixture glyphs
    // and builds the text pipeline emit will look up.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(warm),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect warm");
    let pixels = [RUN_SPLIT_COVERAGE; 4];
    let glyphs = run_split_glyph_inputs(&pixels);
    engine
        .composite_glyphs(
            &mut store,
            &mut platform,
            Dst::server_internal(warm),
            RUN_SPLIT_OP,
            0,
            white,
            &glyphs,
            None,
        )
        .expect("composite_glyphs warm");
    // Closes the frame, which is what commits the atlas inserts.
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(warm),
            full,
            32,
        )
        .expect("get_image warm");

    // Open a frame on the OTHER drawable, so `split_dst` is
    // untouched when the runs are recorded.
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(warm),
            full,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("fill_rect reopen");

    let recorded = run_split_recorded_glyphs(&engine);
    assert_eq!(recorded.len(), 2, "both glyphs must have interned");
    let prior_ticket = store
        .get(split_dst)
        .and_then(|d| d.last_render_ticket.clone());
    let inner = engine.inner.as_mut().expect("inner");
    let pre_op_layout = inner.current_layout_for_drawable(&store, split_dst);
    // Teeth: if the pre-op layout already WERE
    // SHADER_READ_ONLY_OPTIMAL, run 0 and run 1 would carry the
    // same value and the assertions below could not tell a
    // carried pre-op layout from the correct one. Measured
    // `UNDEFINED` here — the case where carrying it onto run 1
    // would license the driver to discard run 0's pixels.
    assert_ne!(
        pre_op_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "the split destination's pre-op layout must differ from where a run ends",
    );
    // First-touch it exactly as the production path does before
    // recording against it.
    {
        let open = inner.frame_builder.open.as_mut().expect("frame open");
        open.touched.first_touch(split_dst, prior_ticket);
        open.layouts.first_touch_drawable(split_dst, pre_op_layout);
    }
    let instances = RenderEngine::record_glyph_runs(
        inner,
        &[&recorded[..1], &recorded[1..]],
        &GlyphRunCommon {
            dst_id: split_dst,
            dst_old_layout: pre_op_layout,
            op: RUN_SPLIT_OP,
            dst_has_alpha: dst_has_alpha_for_pict_format(vk::Format::B8G8R8A8_UNORM, 32, 0),
            foreground_rgba: white,
            clip_scissors: vec![full],
        },
    )
    .expect("record_glyph_runs");
    assert_eq!(instances, 2, "both glyphs must have become instances");
    store.mark_contents_modified(split_dst);

    let layouts: Vec<vk::ImageLayout> = engine
        .inner
        .as_ref()
        .expect("inner")
        .frame_builder
        .open
        .as_ref()
        .expect("frame still open")
        .ops
        .iter()
        .filter_map(|op| match op {
            super::super::frame_builder::RecordedOp::CompositeGlyphs(cg)
                if cg.dst_id == split_dst =>
            {
                Some(cg.dst_old_layout)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        layouts,
        vec![pre_op_layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL],
        "run 0 carries the request's pre-op layout; run 1 carries where run 0 left the image",
    );

    // The recorded barriers must also be emittable: close the
    // frame and submit them.
    engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(split_dst),
            full,
            32,
        )
        .expect("get_image closes the two-run frame");
    engine.drain_all(&mut platform);
}
