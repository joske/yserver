//! Primitive types and conversions for RENDER `Trapezoids` and
//! `Triangles`.
//!
//! Rasterization is GPU-side (see
//! [`trap_pipeline`](super::super::trap_pipeline)) — the per-instance
//! geometry is uploaded as a vertex buffer and a single instanced
//! draw writes coverage into the `R8_UNORM`
//! [`MaskScratch`](super::super::mask_scratch::MaskScratch) image
//! via analytic edge coverage in the fragment shader. The old CPU
//! 4×4 supersampled rasterizer was retired in gpu-trap T5.
//!
//! This module now holds:
//!   * the wire-shaped [`Trapezoid`] / [`Triangle`] primitives
//!     (16.16 fixed-point fields matching `pixman_trapezoid_t` /
//!     `pixman_triangle_t`);
//!   * `to_instance_data` converters that produce the
//!     `TrapInstanceData` / `TriangleInstanceData` GPU structs;
//!   * [`trapezoid_bbox`] / [`triangle_bbox`] — cheap CPU-side bbox
//!     computation. The GPU draw still needs the viewport extent
//!     and scissor rect; the bbox is computed here from the
//!     primitive list before the recording closure runs.
//!
//! Coordinate convention: input rationals are X11 RENDER fixed-point
//! 16.16 (i32). The GPU pipeline consumes f32 in absolute pixel
//! coordinates; the [`fixed_to_f32`] helper does the conversion.

/// 16.16 → f32. Saturates if the input is wildly out of range; doesn't
/// matter for trapezoid rasterisation since the GPU draw clips to the
/// bbox supplied as a scissor rect.
fn fixed_to_f32(v: i32) -> f32 {
    (v as f32) / 65536.0
}

/// One RENDER trapezoid (matches `pixman_trapezoid_t` field-for-field
/// after fixed-point conversion).
#[derive(Debug, Clone, Copy)]
pub struct Trapezoid {
    pub top: i32,    // 16.16 fixed-point
    pub bottom: i32, // 16.16 fixed-point
    pub left_p1: (i32, i32),
    pub left_p2: (i32, i32),
    pub right_p1: (i32, i32),
    pub right_p2: (i32, i32),
}

impl Trapezoid {
    /// Convert this trapezoid (16.16 fixed-point) into the f32-based
    /// instance struct the GPU pipeline (`trap_pipeline::TrapPipeline`)
    /// expects as per-instance vertex attributes (gpu-trap T1).
    #[must_use]
    pub fn to_instance_data(&self) -> crate::kms::vk::trap_pipeline::TrapInstanceData {
        crate::kms::vk::trap_pipeline::TrapInstanceData {
            top: fixed_to_f32(self.top),
            bottom: fixed_to_f32(self.bottom),
            left_p1: [fixed_to_f32(self.left_p1.0), fixed_to_f32(self.left_p1.1)],
            left_p2: [fixed_to_f32(self.left_p2.0), fixed_to_f32(self.left_p2.1)],
            right_p1: [fixed_to_f32(self.right_p1.0), fixed_to_f32(self.right_p1.1)],
            right_p2: [fixed_to_f32(self.right_p2.0), fixed_to_f32(self.right_p2.1)],
        }
    }
}

/// One RENDER triangle (matches `pixman_triangle_t`).
#[derive(Debug, Clone, Copy)]
pub struct Triangle {
    pub p1: (i32, i32),
    pub p2: (i32, i32),
    pub p3: (i32, i32),
}

impl Triangle {
    /// Convert this triangle (16.16 fixed-point) into the f32-based
    /// instance struct the GPU pipeline
    /// (`trap_pipeline::TrapPipeline::triangle_pipeline`) expects as
    /// per-instance vertex attributes (gpu-trap T3).
    #[must_use]
    pub fn to_instance_data(&self) -> crate::kms::vk::trap_pipeline::TriangleInstanceData {
        crate::kms::vk::trap_pipeline::TriangleInstanceData {
            p1: [fixed_to_f32(self.p1.0), fixed_to_f32(self.p1.1)],
            p2: [fixed_to_f32(self.p2.0), fixed_to_f32(self.p2.1)],
            p3: [fixed_to_f32(self.p3.0), fixed_to_f32(self.p3.1)],
        }
    }
}

/// Bounding box (integer pixels). Inclusive lower bound, exclusive
/// upper. Returns `None` if any trapezoid is degenerate / empty —
/// caller treats that as "no draw".
pub fn trapezoid_bbox(traps: &[Trapezoid]) -> Option<(i32, i32, i32, i32)> {
    if traps.is_empty() {
        return None;
    }
    let mut x0 = f32::INFINITY;
    let mut y0 = f32::INFINITY;
    let mut x1 = f32::NEG_INFINITY;
    let mut y1 = f32::NEG_INFINITY;
    for t in traps {
        let top = fixed_to_f32(t.top);
        let bot = fixed_to_f32(t.bottom);
        if bot <= top {
            continue;
        }
        let lp1 = (fixed_to_f32(t.left_p1.0), fixed_to_f32(t.left_p1.1));
        let lp2 = (fixed_to_f32(t.left_p2.0), fixed_to_f32(t.left_p2.1));
        let rp1 = (fixed_to_f32(t.right_p1.0), fixed_to_f32(t.right_p1.1));
        let rp2 = (fixed_to_f32(t.right_p2.0), fixed_to_f32(t.right_p2.1));
        for &p in &[lp1, lp2, rp1, rp2] {
            x0 = x0.min(p.0);
            x1 = x1.max(p.0);
        }
        y0 = y0.min(top);
        y1 = y1.max(bot);
    }
    if !x0.is_finite() || x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        x0.floor() as i32,
        y0.floor() as i32,
        x1.ceil() as i32,
        y1.ceil() as i32,
    ))
}

pub fn triangle_bbox(tris: &[Triangle]) -> Option<(i32, i32, i32, i32)> {
    if tris.is_empty() {
        return None;
    }
    let mut x0 = f32::INFINITY;
    let mut y0 = f32::INFINITY;
    let mut x1 = f32::NEG_INFINITY;
    let mut y1 = f32::NEG_INFINITY;
    for t in tris {
        for p in [t.p1, t.p2, t.p3] {
            let x = fixed_to_f32(p.0);
            let y = fixed_to_f32(p.1);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if !x0.is_finite() || x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        x0.floor() as i32,
        y0.floor() as i32,
        x1.ceil() as i32,
        y1.ceil() as i32,
    ))
}
