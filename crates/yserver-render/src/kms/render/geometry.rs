//! CPU-side geometry for the draw and RENDER lowering: thin-line and
//! polygon rasterisation to `Rectangle16` spans, and the RENDER repeat /
//! affine-transform conversions for the shaders.

use crate::kms::render::cpu_types::{PictTransform, Rectangle16, Repeat};

/// Append rects covering a thin line from (x0,y0) to (x1,y1). Axis-aligned
/// (horizontal / vertical) segments emit a single span rect; diagonals emit
/// one 1×1 rect per Bresenham pixel. Pixel coverage matches a per-pixel walk.
pub fn bresenham_segment(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut Vec<Rectangle16>) {
    // Axis-aligned fast path: a horizontal or vertical thin line is ONE span,
    // not N per-pixel 1×1 rects. This is the overwhelmingly common case
    // (rectangle edges, borders, grids) and matches Xorg's fb layer, which
    // fills H/V lines as a single span. Pixel coverage is identical to the
    // per-pixel loop below; only the rect count shrinks. Emitting per-pixel
    // here previously exploded rectangle outlines into thousands of rects
    // (e.g. blew the root-overlay rect cap, making wide XOR rubber-bands
    // vanish) and cost one GPU draw per pixel.
    let clamp16 = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    if y0 == y1 {
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let w = u16::try_from(xb - xa + 1).unwrap_or(u16::MAX);
        out.push(Rectangle16 {
            x: clamp16(xa),
            y: clamp16(y0),
            width: w,
            height: 1,
        });
        return;
    }
    if x0 == x1 {
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        let h = u16::try_from(yb - ya + 1).unwrap_or(u16::MAX);
        out.push(Rectangle16 {
            x: clamp16(x0),
            y: clamp16(ya),
            width: 1,
            height: h,
        });
        return;
    }
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        out.push(Rectangle16 {
            x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            width: 1,
            height: 1,
        });
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Scanline fill a polygon (even-odd rule).  Edges are pairs of i32
/// vertices.  Output is a Vec of 1-pixel-tall horizontal Rectangle16 spans.
pub fn scanline_fill_polygon(verts: &[(i32, i32)], out: &mut Vec<Rectangle16>) {
    if verts.len() < 3 {
        return;
    }
    let y_min = verts.iter().map(|&(_, y)| y).min().unwrap();
    let y_max = verts.iter().map(|&(_, y)| y).max().unwrap();
    let mut crossings: Vec<i32> = Vec::with_capacity(verts.len());
    for y in y_min..=y_max {
        crossings.clear();
        for i in 0..verts.len() {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % verts.len()];
            // Skip horizontal edges; use half-open [min_y, max_y) so
            // shared vertices contribute exactly once across two edges.
            let (ya, yb) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
            if ya == yb || y < ya || y >= yb {
                continue;
            }
            // Linear interpolation: x at scanline y.
            let x = x0 as i64 + ((y - y0) as i64 * (x1 - x0) as i64) / (y1 - y0) as i64;
            crossings.push(x as i32);
        }
        crossings.sort_unstable();
        let mut i = 0;
        while i + 1 < crossings.len() {
            let x_start = crossings[i];
            let x_end = crossings[i + 1];
            if x_end > x_start {
                let w = (x_end - x_start) as i64;
                out.push(Rectangle16 {
                    x: x_start.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    width: w.min(u16::MAX as i64) as u16,
                    height: 1,
                });
            }
            i += 2;
        }
    }
}

/// Translate a [`Repeat`] enum value to the integer constant the
/// `render.frag.glsl` shader expects (matches the protocol numbering;
/// see `render_pipeline::REPEAT_*`).
pub(crate) fn repeat_to_shader_const(repeat: Repeat) -> i32 {
    use crate::kms::vk::render_pipeline::{REPEAT_NONE, REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT};
    match repeat {
        Repeat::None => REPEAT_NONE,
        Repeat::Normal => REPEAT_NORMAL,
        Repeat::Pad => REPEAT_PAD,
        Repeat::Reflect => REPEAT_REFLECT,
    }
}

/// Convert an X11 RENDER pixman 3×3 transform (16.16 fixed-point) into
/// the affine 2×3 form the `render.frag.glsl` shader uses. RENDER's
/// transform maps the *destination*-relative source coordinate to the
/// pre-sample source pixel:
///
/// ```text
///   src_pixel = M * (src_origin + dst_offset, 1)
/// ```
///
/// We assume affine — the bottom row is `[0, 0, 1]`. Real X11 clients
/// use affine transforms in practice; projective transforms (the rare
/// case) round-trip through the affine portion only and produce wrong
/// pixels at the perspective-divide corners. That trade-off is
/// documented in `feedback_phase4_1_4_decisions.md` § component-alpha
/// and matches the per-family-port strict-acceptance relaxation.
/// Compose two affine 2×3 transforms. The result satisfies
/// `compose(A, B) * v == A * (B * v)` when `v` is `(x, y, 1)`.
pub(crate) fn compose_affines(
    a: crate::kms::vk::ops::render::AffineXform,
    b: crate::kms::vk::ops::render::AffineXform,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    // a.row0 = (a00, a01, a02), a.row1 = (a10, a11, a12). Bottom row
    // implicit `[0, 0, 1]`. Same for b.
    let a00 = a.row0[0];
    let a01 = a.row0[1];
    let a02 = a.row0[2];
    let a10 = a.row1[0];
    let a11 = a.row1[1];
    let a12 = a.row1[2];
    let b00 = b.row0[0];
    let b01 = b.row0[1];
    let b02 = b.row0[2];
    let b10 = b.row1[0];
    let b11 = b.row1[1];
    let b12 = b.row1[2];
    AffineXform {
        row0: [
            a00 * b00 + a01 * b10,
            a00 * b01 + a01 * b11,
            a00 * b02 + a01 * b12 + a02,
            0.0,
        ],
        row1: [
            a10 * b00 + a11 * b10,
            a10 * b01 + a11 * b11,
            a10 * b02 + a11 * b12 + a12,
            0.0,
        ],
    }
}

pub(crate) fn pixman_transform_to_affine(
    transform: Option<&PictTransform>,
    _src_extent: ash::vk::Extent2D,
) -> crate::kms::vk::ops::render::AffineXform {
    use crate::kms::vk::ops::render::AffineXform;
    let Some(t) = transform else {
        return AffineXform::IDENTITY;
    };
    // pixman_transform stores 9 fixed-point i32 values in row-major
    // order. matrix[row][col] in 16.16 fixed point.
    let m = t.matrix;
    let to_f = |v: i32| (v as f32) / 65536.0;
    let mut a = to_f(m[0][0]);
    let mut b = to_f(m[0][1]);
    let mut tx = to_f(m[0][2]);
    let mut c = to_f(m[1][0]);
    let mut d = to_f(m[1][1]);
    let mut ty = to_f(m[1][2]);
    // Constant-divisor projective transforms (matrix row 2 = `[0, 0, w]`
    // with w ≠ 1) collapse to a uniform 1/w scale on the affine portion.
    // Rendercheck's tscoords/tmcoords cases use this form to scale a 5×5
    // src 8×; pixman handles it the same way. Non-constant projective
    // transforms (m[2][0] or m[2][1] non-zero) genuinely vary per-pixel
    // and we don't model them — the affine portion is used as-is, which
    // matches the strict-acceptance relaxation in
    // `feedback_phase4_1_4_decisions.md`.
    let m20 = to_f(m[2][0]);
    let m21 = to_f(m[2][1]);
    let m22 = to_f(m[2][2]);
    if m20 == 0.0 && m21 == 0.0 && m22 != 0.0 && m22 != 1.0 {
        let inv = 1.0 / m22;
        a *= inv;
        b *= inv;
        tx *= inv;
        c *= inv;
        d *= inv;
        ty *= inv;
    }
    AffineXform {
        row0: [a, b, tx, 0.0],
        row1: [c, d, ty, 0.0],
    }
}
