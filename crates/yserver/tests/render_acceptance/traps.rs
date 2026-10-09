use super::*;

/// Stage 3e.2 acceptance: a 4×4 axis-aligned trapezoid (= filled
/// rect) painted via `render_trapezoids` must produce full coverage
/// in the trap interior. Validates the entire GPU pipeline: trap
/// rasterize → mask scratch → composite with SolidFill src. v1
/// has the equivalent rendercheck-driven gate; this is the v2
/// in-tree oracle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_trapezoids_renders_filled_rect() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 8, 8).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("pre-fill blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // 16.16 fixed-point axis-aligned trapezoid:
    // top=2, bottom=6, left x=2, right x=6 → 4×4 inset rect.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        2 << 16, // top
        6 << 16, // bottom
        2 << 16, // left_p1.x
        2 << 16, // left_p1.y
        2 << 16, // left_p2.x
        6 << 16, // left_p2.y
        6 << 16, // right_p1.x
        2 << 16, // right_p1.y
        6 << 16, // right_p2.x
        6 << 16, // right_p2.y
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }

    b.render_trapezoids(
        None,
        3, // Over
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0, // mask_format — ignored at parity scope
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("Some");
    // Trap interior pixel (3, 3) — solidly inside — must be red.
    let off_inside = (3 * 8 + 3) * 4;
    assert_eq!(
        &out[off_inside..off_inside + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "trap interior should be red (got {:?})",
        &out[off_inside..off_inside + 4],
    );
    // Outside the trap (0, 0) must stay blue.
    assert_eq!(
        &out[0..4],
        &[0xFF, 0x00, 0x00, 0xFF],
        "outside trap should stay blue (got {:?})",
        &out[0..4],
    );
}

/// Repro for the xeyes "pupils missing" hardware-smoke bug
/// reported 2026-05-16. xeyes paints:
///
/// 1. Trapezoids op=Over src=<SolidFill white> at the eye region
/// 2. Trapezoids op=Over src=<SolidFill black> at a smaller
///    pupil region inside it
///
/// On hardware the eye whites render correctly but the black
/// pupils never appear. v1's PaintBatch coalesces multiple paints
/// into ONE CB with in-CB barriers; v2's per-op CB shape means each
/// `render_trapezoids` call has its own CB. Both CBs share the
/// engine's single 1×1 `solid_src_image` scratch — CB1 clears it
/// to white + samples, CB2 clears it to black + samples. Hypothesis:
/// the cross-CB barrier on `solid_src_image` either isn't strong
/// enough to prevent CB2's clear from racing CB1's sample, or some
/// other piece of state is shared without proper sync.
///
/// Test: 16×16 dst pre-filled green; an 8×8 axis-aligned white
/// trap, then a 4×4 axis-aligned black trap inside it. The final
/// dst should read:
///
/// - black at the centre (inside both traps)
/// - white between (inside white but outside black)
/// - green at corners (outside both traps)
///
/// If the second paint loses its black source (race on
/// `solid_src_image`), the centre will read white or undefined.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn back_to_back_trapezoids_different_solidfill_colors() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 16, 16).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    // Pre-fill green: 0xFF00FF00 ARGB. BGRA wire bytes:
    // B=0, G=0xFF, R=0, A=0xFF.
    b.fill_rectangle(None, dst_xid, 0xFF00FF00, 0, 0, 16, 16)
        .expect("pre-fill green");

    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // White SolidFill: RGBA(0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF).
    let white_src = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill white")
        .expect("Some");
    // Black SolidFill: RGBA(0, 0, 0, 0xFFFF).
    let black_src = b
        .render_create_solid_fill(None, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill black")
        .expect("Some");

    // Helper: build an axis-aligned trapezoid wire blob (40 bytes
    // per trap, 16.16 fixed-point).
    let trap_bytes = |top: i32, bot: i32, left: i32, right: i32| -> Vec<u8> {
        let mut v: Vec<u8> = Vec::with_capacity(40);
        let fields: [i32; 10] = [
            top << 16,
            bot << 16,
            left << 16,
            top << 16,
            left << 16,
            bot << 16,
            right << 16,
            top << 16,
            right << 16,
            bot << 16,
        ];
        for f in fields {
            v.extend_from_slice(&f.to_le_bytes());
        }
        v
    };

    // 8×8 white trap at (4..12, 4..12) — analogous to xeyes' eye
    // white.
    b.render_trapezoids(
        None,
        3, // Over
        white_src.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &trap_bytes(4, 12, 4, 12),
        0,
        0,
    )
    .expect("render_trapezoids white");

    // 4×4 black trap at (6..10, 6..10) — analogous to xeyes' pupil
    // inside the eye.
    b.render_trapezoids(
        None,
        3, // Over
        black_src.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &trap_bytes(6, 10, 6, 10),
        0,
        0,
    )
    .expect("render_trapezoids black");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 16, 16, !0)
        .expect("get_image")
        .expect("Some");

    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 16 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Centre (8, 8): inside black trap → must read black.
    assert_eq!(
        pixel(8, 8),
        [0x00, 0x00, 0x00, 0xFF],
        "centre must be black (pupil): {:?} — if white, the second \
         render_trapezoids' SolidFill source was lost (shared \
         solid_src_image race?)",
        pixel(8, 8),
    );
    // (5, 5): inside white but outside black → must read white.
    assert_eq!(
        pixel(5, 5),
        [0xFF, 0xFF, 0xFF, 0xFF],
        "(5,5) must be white (eye): got {:?}",
        pixel(5, 5),
    );
    // (1, 1): outside both → must stay green.
    assert_eq!(
        pixel(1, 1),
        [0x00, 0xFF, 0x00, 0xFF],
        "(1,1) must stay green (root bg): got {:?}",
        pixel(1, 1),
    );
}

/// xeyes "stripes-in-the-eye-white" repro. xeyes builds each eye
/// out of ~16 stacked horizontal trapezoids that share their
/// top/bottom edges (trap N's bottom = trap N+1's top). The shared
/// edge sits on a non-integer Y coordinate (xeyes' ellipse math
/// rounds to fixed-point 16.16). For pixels straddling the
/// boundary, the AA edge formula must produce coverages from the
/// two adjacent traps that SUM to ~1.0 — otherwise the boundary
/// rows under-cover and you see horizontal stripes inside the
/// eye whites.
///
/// Pre-3f.x fix: trap.frag.glsl's `c_top` / `c_bot` formulas
/// computed `clamp(p.y - top, 0, 1)` instead of
/// `clamp(0.5 + (p.y - top), 0, 1)` — off by 0.5 vs the slanted-
/// edge formula. At a shared boundary y=12.788, pixel center
/// y=12.5: trap1 c_bot = clamp(0.288, 0, 1) = 0.288; trap2 c_top
/// = clamp(-0.288, 0, 1) = 0; total = 0.288, leaving 0.712
/// missing coverage at that row.
///
/// Test: two adjacent axis-aligned traps sharing y=4.5. Centre
/// row (y=4) should read fully opaque white.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn adjacent_trapezoids_share_horizontal_boundary_cleanly() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 10, 10).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 10, 10)
        .expect("pre-fill blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill white")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // Two adjacent trapezoids sharing y=4.5 boundary.
    // Both span x∈[2, 8].
    // 16.16 fixed-point: pixel * 65536; half-pixel = 32768.
    let fields1: [i32; 10] = [
        2 << 16,            // top = 2
        (4 << 16) | 0x8000, // bottom = 4.5
        2 << 16,
        2 << 16,
        2 << 16,
        (4 << 16) | 0x8000,
        8 << 16,
        2 << 16,
        8 << 16,
        (4 << 16) | 0x8000,
    ];
    let fields2: [i32; 10] = [
        (4 << 16) | 0x8000, // top = 4.5
        7 << 16,            // bottom = 7
        2 << 16,
        (4 << 16) | 0x8000,
        2 << 16,
        7 << 16,
        8 << 16,
        (4 << 16) | 0x8000,
        8 << 16,
        7 << 16,
    ];
    let mut traps: Vec<u8> = Vec::with_capacity(80);
    for v in fields1 {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    for v in fields2 {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    b.render_trapezoids(
        None,
        3,
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 10, 10, !0)
        .expect("get_image")
        .expect("Some");
    let pixel = |x: usize, y: usize| -> [u8; 4] {
        let off = (y * 10 + x) * 4;
        [out[off], out[off + 1], out[off + 2], out[off + 3]]
    };

    // Row 4 (centre y=4.5, straddles the trap boundary). Should
    // read white (≈ full coverage). Pre-fix: ≈ partial coverage,
    // pixel is mostly white but blended with blue under-fill →
    // visible stripe.
    for x in 3..7 {
        let p = pixel(x, 4);
        // Each channel near 0xFF (allow ±16 for AA softening at
        // slanted side edges — but x=3..7 is well-inside the
        // trapezoid horizontally so the slanted-edge AA is full).
        assert!(
            p[0] >= 0xE0 && p[1] >= 0xE0 && p[2] >= 0xE0,
            "row 4 should be ~white at x={x} (got {:?}); pre-fix bug = horizontal stripe",
            p,
        );
    }
}

/// Diagnostic: same trap geometry shape as
/// render_trapezoids_renders_filled_rect but with a LARGE bbox
/// (covering most of mask_scratch's 256×256 default extent). If
/// this passes while the 4×4 variant fails, the bug is
/// bbox-size-vs-mask-extent ratio — Intel rasterizer culls tiny
/// quads in big viewports. The fix would be to size the viewport
/// to the bbox, not the full mask.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_trapezoids_large_bbox_repro() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // 200×200 dst pre-filled blue.
    let dst_pix = b.create_pixmap(None, 32, 200, 200).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 200, 200)
        .expect("fill pre-blue");

    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid_fill red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");

    // Big axis-aligned trap: 100×100 inside the 200×200 dst.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        50 << 16,
        150 << 16,
        50 << 16,
        50 << 16,
        50 << 16,
        150 << 16,
        150 << 16,
        50 << 16,
        150 << 16,
        150 << 16,
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }
    b.render_trapezoids(
        None,
        3,
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        0,
        0,
        &traps,
        0,
        0,
    )
    .expect("render_trapezoids");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 200, 200, !0)
        .expect("get_image")
        .expect("Some");
    // Center pixel (100, 100) — well inside trap (50..150, 50..150).
    let off = (100 * 200 + 100) * 4;
    assert_eq!(
        &out[off..off + 4],
        &[0x00, 0x00, 0xFF, 0xFF],
        "center should be red (got {:?})",
        &out[off..off + 4],
    );
}

/// Deterministic xorshift for the trap/triangle stress sets.
struct StressRng(u64);

impl StressRng {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        #[allow(clippy::cast_precision_loss)]
        let v = (self.0 >> 40) as f32 / (1u64 << 24) as f32;
        v
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }
}

#[allow(clippy::cast_possible_truncation)]
fn stress_fixed(v: f32) -> i32 {
    (f64::from(v) * 65536.0).round() as i32
}

const STRESS_W: u32 = 2600;
const STRESS_H: u32 = 2000;

/// #214 shape: thousands of thin slanted trapezoids over a ~2400×1800
/// bbox, plus large ones and the degenerate shapes the shader still
/// paints or zeroes (flat, inverted, zero-length/horizontal/upward
/// edges, crossed sides, partly off the pixmap). 16.16 fields in wire
/// order: top, bottom, left p1/p2, right p1/p2.
fn trap_stress_set() -> Vec<[i32; 10]> {
    let mut rng = StressRng(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<[f32; 10]> = Vec::new();
    for _ in 0..8000 {
        let x = rng.range(-20.0, 2420.0);
        let y = rng.range(-20.0, 1820.0);
        let h = rng.range(0.25, 6.0);
        let w = rng.range(0.2, 3.0);
        let slant = rng.range(-15.0, 15.0);
        let (e1, e2) = (rng.range(0.0, 8.0), rng.range(0.0, 8.0));
        let k = slant / h;
        out.push([
            y,
            y + h,
            x - k * e1,
            y - e1,
            x + slant + k * e2,
            y + h + e2,
            x + w - k * e1,
            y - e1,
            x + w + slant + k * e2,
            y + h + e2,
        ]);
    }
    for _ in 0..24 {
        let x = rng.range(0.0, 2000.0);
        let y = rng.range(0.0, 1500.0);
        let h = rng.range(100.0, 400.0);
        let w = rng.range(50.0, 500.0);
        let (sl, sr) = (rng.range(-200.0, 200.0), rng.range(-200.0, 200.0));
        out.push([y, y + h, x, y, x + sl, y + h, x + w, y, x + w + sr, y + h]);
    }
    // Near-horizontal sides: the AA band runs far along x.
    for i in 0..16 {
        #[allow(clippy::cast_precision_loss)]
        let y = 100.0 + 90.0 * i as f32;
        out.push([
            y,
            y + 3.0,
            300.0,
            y,
            700.0,
            y + 0.7,
            900.0,
            y,
            1300.0,
            y + 2.0,
        ]);
    }
    out.extend_from_slice(&[
        // flat (top == bottom) and inverted (bottom < top)
        [
            500.25, 500.25, 100.0, 490.0, 100.0, 510.0, 400.0, 490.0, 400.0, 510.0,
        ],
        [
            600.5, 600.0, 100.0, 590.0, 100.0, 610.0, 400.0, 590.0, 400.0, 610.0,
        ],
        // zero-length left edge
        [
            700.0, 720.0, 150.0, 710.0, 150.0, 710.0, 300.0, 700.0, 300.0, 720.0,
        ],
        // horizontal left edge
        [
            800.0, 830.0, 100.0, 815.0, 140.0, 815.0, 300.0, 800.0, 300.0, 830.0,
        ],
        // upward-directed edges, each side and both
        [
            900.0, 940.0, 200.0, 940.0, 180.0, 900.0, 400.0, 900.0, 420.0, 940.0,
        ],
        [
            1000.0, 1040.0, 200.0, 1000.0, 180.0, 1040.0, 400.0, 1040.0, 420.0, 1000.0,
        ],
        [
            1100.0, 1140.0, 200.0, 1140.0, 180.0, 1100.0, 400.0, 1140.0, 420.0, 1100.0,
        ],
        // crossed sides (left right of right)
        [
            1200.0, 1240.0, 600.0, 1200.0, 610.0, 1240.0, 500.0, 1200.0, 505.0, 1240.0,
        ],
        // sides crossing mid-trapezoid
        [
            1300.0, 1360.0, 500.0, 1300.0, 700.0, 1360.0, 700.0, 1300.0, 500.0, 1360.0,
        ],
        // partly off the pixmap on every side
        [
            -30.0, 40.0, -50.0, -30.0, -20.0, 40.0, 60.0, -30.0, 90.0, 40.0,
        ],
        [
            1960.0, 2030.0, 2550.0, 1960.0, 2580.0, 2030.0, 2650.0, 1960.0, 2690.0, 2030.0,
        ],
        // a 1/65536-wide sliver and a sub-pixel speck
        [
            1500.0, 1510.0, 800.0, 1500.0, 800.0, 1510.0, 800.000_02, 1500.0, 800.000_02, 1510.0,
        ],
        [
            1520.3, 1520.6, 810.2, 1520.0, 810.2, 1521.0, 810.7, 1520.0, 810.7, 1521.0,
        ],
    ]);
    out.iter().map(|t| t.map(stress_fixed)).collect()
}

/// Thin, large, sharp-angled and collinear triangles (wire order
/// p1, p2, p3 as 16.16 x/y pairs).
fn triangle_stress_set() -> Vec<[i32; 6]> {
    let mut rng = StressRng(0xD1B5_4A32_D192_ED03);
    let mut out: Vec<[f32; 6]> = Vec::new();
    for _ in 0..8000 {
        let x = rng.range(-20.0, 2420.0);
        let y = rng.range(-20.0, 1820.0);
        out.push([
            x,
            y,
            x + rng.range(-12.0, 12.0),
            y + rng.range(-12.0, 12.0),
            x + rng.range(-3.0, 3.0),
            y + rng.range(-3.0, 3.0),
        ]);
    }
    for _ in 0..16 {
        let x = rng.range(0.0, 2000.0);
        let y = rng.range(0.0, 1500.0);
        out.push([
            x,
            y,
            x + rng.range(-400.0, 400.0),
            y + rng.range(0.0, 400.0),
            x + rng.range(-400.0, 400.0),
            y + rng.range(0.0, 400.0),
        ]);
    }
    out.extend_from_slice(&[
        // needle: the dilated tip reaches far past the vertex bbox
        [100.0, 1900.0, 1100.0, 1900.3, 100.0, 1900.6],
        [1200.0, 1700.0, 1200.4, 1900.0, 1200.8, 1700.0],
        // collinear and coincident
        [300.0, 300.0, 400.0, 400.0, 500.0, 500.0],
        [600.0, 600.0, 600.0, 600.0, 600.0, 600.0],
        // partly off the pixmap
        [-40.0, -40.0, 60.0, -10.0, 10.0, 70.0],
        [2560.0, 1950.0, 2700.0, 1990.0, 2590.0, 2100.0],
    ]);
    out.iter().map(|t| t.map(stress_fixed)).collect()
}

/// CPU mirror of `trap.frag.glsl` / `triangle.frag.glsl`'s
/// `edge_coverage_linear`.
fn stress_edge_cov(p: (f32, f32), a: (f32, f32), b: (f32, f32), inside: f32) -> f32 {
    let d = (b.0 - a.0, b.1 - a.1);
    let len = (d.0 * d.0 + d.1 * d.1).sqrt();
    if len < 1e-6 {
        return 0.0;
    }
    let n = (-d.1 / len, d.0 / len);
    let sd = ((p.0 - a.0) * n.0 + (p.1 - a.1) * n.1) * inside;
    (0.5 - sd).clamp(0.0, 1.0)
}

fn stress_fx(v: i32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let f = v as f32 / 65536.0;
    f
}

/// Accumulate one primitive into the CPU oracle mask the way the GPU's
/// ONE+ONE blend into R8 does (clamp, quantize per add). Evaluates the
/// primitive over EVERY column of the rows `rows`, so it does not share
/// the per-instance extent logic under test.
fn stress_accumulate(acc: &mut [u8], rows: std::ops::Range<i64>, cov: impl Fn((f32, f32)) -> f32) {
    let w = i64::from(STRESS_W);
    for y in rows.start.max(0)..rows.end.min(i64::from(STRESS_H)) {
        for x in 0..w {
            #[allow(clippy::cast_precision_loss)]
            let c = cov((x as f32 + 0.5, y as f32 + 0.5));
            if c > 0.0 {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let i = (y * w + x) as usize;
                let v = (c + f32::from(acc[i]) / 255.0).min(1.0);
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                {
                    acc[i] = (v * 255.0).round() as u8;
                }
            }
        }
    }
}

fn trap_oracle(traps: &[[i32; 10]]) -> Vec<u8> {
    let mut acc = vec![0u8; (STRESS_W * STRESS_H) as usize];
    for t in traps {
        let f = t.map(stress_fx);
        let (top, bot) = (f[0], f[1]);
        let (lp1, lp2, rp1, rp2) = ((f[2], f[3]), (f[4], f[5]), (f[6], f[7]), (f[8], f[9]));
        // c_top * c_bot > 0 only for pixel centres in (top-0.5, bottom+0.5).
        #[allow(clippy::cast_possible_truncation)]
        let rows = (top.floor() as i64 - 2)..(bot.ceil() as i64 + 2);
        stress_accumulate(&mut acc, rows, |p| {
            let c_top = (0.5 + (p.1 - top)).clamp(0.0, 1.0);
            let c_bot = (0.5 + (bot - p.1)).clamp(0.0, 1.0);
            c_top * c_bot * stress_edge_cov(p, lp1, lp2, 1.0) * stress_edge_cov(p, rp1, rp2, -1.0)
        });
    }
    acc
}

fn triangle_oracle(tris: &[[i32; 6]]) -> Vec<u8> {
    let mut acc = vec![0u8; (STRESS_W * STRESS_H) as usize];
    for t in tris {
        let f = t.map(stress_fx);
        let (p1, p2, p3) = ((f[0], f[1]), (f[2], f[3]), (f[4], f[5]));
        let area2 = (p2.0 - p1.0) * (p3.1 - p1.1) - (p2.1 - p1.1) * (p3.0 - p1.0);
        if area2.abs() < 1e-3 {
            continue;
        }
        let orient = if area2 > 0.0 { -1.0 } else { 1.0 };
        // Rows: the shader's 0.5px-dilated triangle is the triangle
        // scaled about its incentre by (r + 0.5) / r (r = inradius).
        let (a, bb, c) = (
            f64::from((p2.0 - p3.0).hypot(p2.1 - p3.1)),
            f64::from((p3.0 - p1.0).hypot(p3.1 - p1.1)),
            f64::from((p1.0 - p2.0).hypot(p1.1 - p2.1)),
        );
        let per = a + bb + c;
        let iy = (a * f64::from(p1.1) + bb * f64::from(p2.1) + c * f64::from(p3.1)) / per;
        let r = f64::from(area2.abs()) / per;
        let s = (r + 0.5) / r;
        let ys = [p1.1, p2.1, p3.1].map(|y| iy + s * (f64::from(y) - iy));
        let lo = ys.iter().copied().fold(f64::INFINITY, f64::min).max(-1.0);
        let hi = ys
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
            .min(f64::from(STRESS_H) + 1.0);
        #[allow(clippy::cast_possible_truncation)]
        let rows = (lo.floor() as i64 - 2)..(hi.ceil() as i64 + 2);
        stress_accumulate(&mut acc, rows, |p| {
            stress_edge_cov(p, p1, p2, orient)
                * stress_edge_cov(p, p2, p3, orient)
                * stress_edge_cov(p, p3, p1, orient)
        });
    }
    acc
}

/// Paint `wire` (Trapezoids or Triangles bytes) with op=Add, white
/// solid src, into a cleared A8 pixmap and read the coverage back.
/// Prints the wall time of the op (submit + wait, minus a bare readback).
fn stress_paint(b: &mut KmsBackend, triangles: bool, wire: &[u8]) -> Vec<u8> {
    let dst_pix = b
        .create_pixmap(None, 8, STRESS_W as u16, STRESS_H as u16)
        .expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst_pic")
        .expect("Some");
    let (w, h) = (STRESS_W as u16, STRESS_H as u16);
    // Warm-up: the first primitive alone, so pipeline creation and
    // scratch growth stay out of the timed op.
    let first = if triangles { &wire[..24] } else { &wire[..40] };
    if triangles {
        b.render_triangles_op(
            None,
            11,
            12,
            src_pic.as_raw(),
            dst_pic.as_raw(),
            0,
            0,
            0,
            first,
            0,
            0,
        )
        .expect("warm-up");
    } else {
        b.render_trapezoids(
            None,
            12,
            src_pic.as_raw(),
            dst_pic.as_raw(),
            0,
            0,
            0,
            first,
            0,
            0,
        )
        .expect("warm-up");
    }
    b.fill_rectangle(None, dst_xid, 0, 0, 0, w, h)
        .expect("clear");
    // Drain the warm-up and the clear before the timed op.
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 1, 1, !0)
        .expect("get_image");
    let t1 = std::time::Instant::now();
    if triangles {
        b.render_triangles_op(
            None,
            11,
            12,
            src_pic.as_raw(),
            dst_pic.as_raw(),
            0,
            0,
            0,
            wire,
            0,
            0,
        )
        .expect("render_triangles");
    } else {
        b.render_trapezoids(
            None,
            12,
            src_pic.as_raw(),
            dst_pic.as_raw(),
            0,
            0,
            0,
            wire,
            0,
            0,
        )
        .expect("render_trapezoids");
    }
    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, w, h, !0)
        .expect("get_image")
        .expect("Some");
    let total = t1.elapsed();
    let t2 = std::time::Instant::now();
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, w, h, !0)
        .expect("get_image");
    let readback = t2.elapsed();
    eprintln!(
        "#214 stress {}: op+readback {:.1} ms (idle readback alone {:.1} ms)",
        if triangles { "triangles" } else { "trapezoids" },
        total.as_secs_f64() * 1e3,
        readback.as_secs_f64() * 1e3,
    );
    out
}

/// Compare the painted coverage against the CPU oracle and optionally
/// dump the raw bytes (`YSERVER_TRAP_STRESS_DUMP=<dir>`) for an
/// out-of-band A/B between builds.
fn stress_check(name: &str, got: &[u8], want: &[u8]) {
    if let Some(dir) = std::env::var_os("YSERVER_TRAP_STRESS_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("{name}.r8"));
        std::fs::write(&path, got).expect("dump");
        eprintln!("dumped {}", path.display());
    }
    assert_eq!(got.len(), want.len());
    let mut worst = (0u8, 0usize);
    let mut off = 0usize;
    let mut painted = 0usize;
    for (i, (&g, &w)) in got.iter().zip(want).enumerate() {
        let d = g.abs_diff(w);
        if d > worst.0 {
            worst = (d, i);
        }
        if d > 2 {
            off += 1;
        }
        if g > 0 {
            painted += 1;
        }
    }
    let (x, y) = (worst.1 % STRESS_W as usize, worst.1 / STRESS_W as usize);
    eprintln!(
        "#214 stress {name}: {painted} painted px, {off} px off the oracle by >2, \
         worst {} at ({x},{y}) got {} want {}",
        worst.0, got[worst.1], want[worst.1]
    );
    assert_eq!(off, 0, "{name}: coverage diverges from the CPU oracle");
}

/// #214: a CompositeTrapezoids with ~8000 thin trapezoids must paint the
/// same coverage as the shader math evaluated per pixel — in particular
/// no AA edge pixel may be lost by the per-instance raster extent.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn trapezoid_stress_matches_cpu_coverage_oracle() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let traps = trap_stress_set();
    let wire: Vec<u8> = traps
        .iter()
        .flat_map(|t| t.iter().flat_map(|v| v.to_le_bytes()))
        .collect();
    let got = stress_paint(&mut b, false, &wire);
    stress_check("trapezoids", &got, &trap_oracle(&traps));
}

/// #214 sibling: the Triangles path shares the instanced mask pass.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn triangle_stress_matches_cpu_coverage_oracle() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let tris = triangle_stress_set();
    let wire: Vec<u8> = tris
        .iter()
        .flat_map(|t| t.iter().flat_map(|v| v.to_le_bytes()))
        .collect();
    let got = stress_paint(&mut b, true, &wire);
    stress_check("triangles", &got, &triangle_oracle(&tris));
}
