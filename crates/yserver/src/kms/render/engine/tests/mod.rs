mod core_ops;
mod glyph_atlas;
mod glyph_runs;
mod memory_frame;
mod pixel_clamp;
mod render_composite;
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
    let fence_pool = crate::kms::render::platform::FencePool::new(Arc::clone(&vk));
    p.vk = Some(vk);
    p.ops_command_pool = Some(ops_pool);
    p.fence_pool = Some(fence_pool);
    Some(p)
}

/// Alias of `live_platform` used by Task 3 tests.
fn try_for_tests_with_vk() -> Option<PlatformBackend> {
    live_platform()
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
        crate::kms::render::store::DrawableKind::Pixmap,
        false,
    )
}

fn alloc_drawable_3a_with_kind(
    platform: &PlatformBackend,
    store: &mut DrawableStore,
    xid: u32,
    w: u16,
    h: u16,
    kind: crate::kms::render::store::DrawableKind,
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
