#version 450

// GPU rasterization for RENDER Triangles (gpu-trap T3). Computes
// analytic edge coverage for the three triangle edges (p1→p2,
// p2→p3, p3→p1). Output is single-channel R8 coverage in [0, 1].
//
// AA strategy: linear-approximation edge coverage. Per edge, the
// signed distance from the pixel center to the edge line is clamped
// to [0, 1] (centered at 0.5 from the line). The three contributions
// multiply to the pixel's coverage. Same scheme as the trapezoid
// fragment shader — see `trap.frag.glsl` for the rationale.
//
// Winding-order handling: RENDER does NOT specify CW vs CCW. The
// vertex shader computes `orient` (signed-area sign of p1, p2, p3)
// and passes it through as a flat float (-1 for CCW, +1 for CW,
// 0 for degenerate/collinear). The fragment uses `orient` as the
// `inside_side` argument for all three `edge_coverage_linear` calls,
// which flips the half-plane convention to match the actual triangle
// orientation. This mirrors the CPU reference `point_in_triangle`
// in `vk/ops/traps.rs:304`, which is sign-agnostic — "interior" =
// all three barycentric signs agree (all positive OR all negative).
//
// Sign-flip rationale: with edge perpendicular `n = (-d.y, d.x)`
// (90° CCW from edge direction), interior of a CCW triangle has
// POSITIVE signed_dist for every edge. To make the coverage formula
// `clamp(0.5 - signed_dist * inside_side, 0, 1)` return HIGH coverage
// for interior, we need `signed_dist * inside_side` NEGATIVE → so
// CCW (positive signed_dist) needs inside_side = -1.
//
// Degenerate (collinear) triangle: vertex shader sets `orient = 0.0`
// when |signed_area_2| is below the area-epsilon. Fragment discards.
// (Without this, a slanted collinear triangle with three distinct
// vertices would have nonzero edge lengths and produce non-zero
// product coverage — incorrect; the triangle covers zero area.)
//
// `c_i` from `edge_coverage_linear` is already clamped to [0, 1] —
// DO NOT take `abs(c_i)`. The sign handling lives in the
// `inside_side` parameter, not in the post-clamp result.

layout(location = 0) flat in vec2 p1;
layout(location = 1) flat in vec2 p2;
layout(location = 2) flat in vec2 p3;
layout(location = 3) flat in float orient;

// Triangle corners arrive in ABSOLUTE pixel coords from the X
// protocol, but the quad emits at MaskScratch-LOCAL coords (so the
// GPU writes mask data at (0..bbox_w, 0..bbox_h) matching the CPU
// rasterize convention). `bbox_origin_pixel` is the absolute origin
// of the bbox; the fragment adds it to `gl_FragCoord` to recover
// absolute pixel position for the edge math.
layout(push_constant) uniform PushConsts {
    vec2 mask_extent;
    vec2 bbox_origin_pixel;
    vec2 bbox_size_pixel;
    vec2 _pad;
} pc;

layout(location = 0) out float coverage;

// Coverage contribution of one edge. Same shape as the trapezoid
// shader's helper. `inside_side` is the winding-order sign (+1 / -1)
// computed once per triangle in the vertex shader.
//
// Degenerate (zero-length) edges return 0.0 — a collapsed edge
// means the triangle has no area along this side; the multiplicative
// product collapses to 0, which is correct (a degenerate / collinear
// triangle covers no pixels).
float edge_coverage_linear(vec2 p, vec2 a, vec2 b, float inside_side) {
    vec2 d = b - a;
    float len = length(d);
    if (len < 1e-6) {
        return 0.0;
    }
    vec2 n = vec2(-d.y, d.x) / len;
    float signed_dist = dot(p - a, n) * inside_side;
    return clamp(0.5 - signed_dist, 0.0, 1.0);
}

void main() {
    // Collinear (degenerate) triangle: the vertex shader signals this
    // with orient = 0.0. Discard so the fragment contributes nothing
    // to the coverage mask. (Plain `coverage = 0.0; return;` would
    // also write zero to the framebuffer, defeating the
    // additive-blend "no contribution = leave existing value alone"
    // pattern. `discard` skips the framebuffer write entirely.)
    if (orient == 0.0) {
        discard;
    }

    // Translate fragment-local coords to absolute pixel coords for
    // the edge math (the quad emits in mask-local space to land mask
    // data at MaskScratch[(0,0)..(bbox_w, bbox_h)]).
    vec2 p = gl_FragCoord.xy + pc.bbox_origin_pixel;

    // Three half-plane tests, one per edge. All three use the same
    // `orient` sign so the inside-half is consistent regardless of
    // whether the triangle was wound CW or CCW.
    float c1 = edge_coverage_linear(p, p1, p2, orient);
    float c2 = edge_coverage_linear(p, p2, p3, orient);
    float c3 = edge_coverage_linear(p, p3, p1, orient);

    coverage = c1 * c2 * c3;
}
