#version 450

// GPU rasterization for RENDER Triangles (gpu-trap T3). Emits a
// quad (4 vertices via TRIANGLE_STRIP) per instance covering the
// pixels that triangle can touch, clamped to the per-draw bbox. Per-instance attributes encode the
// triangle's three corners; they are flat-interpolated to the
// fragment stage which computes analytic edge coverage.
//
// Winding-order handling: RENDER does NOT specify a winding
// convention — triangles arrive in either CW or CCW order. The
// vertex shader computes the signed-area of (p1, p2, p3) once per
// instance and forwards its sign as `orient` (flat, -1 / +1 / 0) so the
// fragment shader can pick a consistent inside-side for each edge
// regardless of winding. This mirrors the sign-agnostic CPU
// reference in `vk/ops/traps.rs::point_in_triangle` (which uses
// "all-three-signs-agree" barycentric tests).
//
// Sign convention (load-bearing): with `edge_coverage_linear`'s
// perpendicular `n = (-d.y, d.x)` (90° CCW from edge direction),
// an interior pixel of a CCW-wound triangle has POSITIVE signed_dist
// against each edge (interior is in +n direction). The shader's
// coverage formula `clamp(0.5 - signed_dist * inside_side, 0, 1)`
// returns HIGH coverage when `signed_dist * inside_side < 0`.
// Therefore: for CCW (signed_area_2 > 0), `inside_side = -1`. For
// CW (signed_area_2 < 0), `inside_side = +1`. The previous
// convention (+1 for CCW) was inverted and made positive-area
// triangles render empty (caught by codex T3 review P1).
//
// Degenerate (collinear) triangle handling: when |signed_area_2|
// is below an area-epsilon, the triangle covers zero area. The
// `len < 1e-6` zero-length-edge guard inside `edge_coverage_linear`
// only catches edges that are literally zero-length — a slanted
// collinear triangle with three distinct vertices has nonzero edge
// lengths and would produce nonzero (incorrect) coverage. The
// vertex shader sets `orient = 0.0` for these; the fragment
// shader discards on `orient == 0.0`.

layout(push_constant) uniform PushConsts {
    vec2 mask_extent;        // mask scratch image extent (pixels)
    vec2 bbox_origin_pixel;  // top-left of bbox in ABSOLUTE pixel coords
    vec2 bbox_size_pixel;    // bbox size in pixels
    vec2 _pad;
} pc;

// Per-instance triangle attributes (stride = 24, INSTANCE rate).
layout(location = 0) in vec2 in_p1;
layout(location = 1) in vec2 in_p2;
layout(location = 2) in vec2 in_p3;

layout(location = 0) flat out vec2 p1;
layout(location = 1) flat out vec2 p2;
layout(location = 2) flat out vec2 p3;
// Winding-order sign: +1 for CCW (signed_area_2 >= 0), -1 for CW.
// Used as `inside_side` for all 3 edge_coverage_linear calls in the
// fragment so the half-plane convention matches the triangle's
// actual orientation.
layout(location = 3) flat out float orient;

// #214: each instance's quad covers only the pixels the fragment stage
// can give nonzero coverage, instead of the whole union bbox — the old
// shape cost N × bbox_w × bbox_h fragments and hung Intel HD 500 for
// ~10 s on a ~7,900-trapezoid GTK repaint. The region is the union
// bbox clipped by each edge's half-plane dilated by the fragment's
// 0.5px AA band plus EXTENT_SLACK, so no pixel with coverage > 0 is
// dropped; pixels the quad still covers but the primitive misses
// compute 0 and add 0 under the ONE+ONE blend, so the mask stays
// bit-identical.
const float EXTENT_SLACK = 1.0;
const int MAX_POLY = 8;
vec2 poly[MAX_POLY];
int poly_n;

// Keep the part of `poly` where dot(p - a, n) <= lim (Sutherland-Hodgman).
void clip_half_plane(vec2 a, vec2 n, float lim) {
    vec2 outp[MAX_POLY];
    int out_n = 0;
    for (int i = 0; i < poly_n; i++) {
        vec2 cur = poly[i];
        vec2 prv = poly[(i + poly_n - 1) % poly_n];
        float dc = dot(cur - a, n) - lim;
        float dp = dot(prv - a, n) - lim;
        if ((dc <= 0.0) != (dp <= 0.0) && out_n < MAX_POLY) {
            outp[out_n++] = prv + (cur - prv) * (dp / (dp - dc));
        }
        if (dc <= 0.0 && out_n < MAX_POLY) {
            outp[out_n++] = cur;
        }
    }
    for (int i = 0; i < out_n; i++) {
        poly[i] = outp[i];
    }
    poly_n = out_n;
}

// Clip by the half-plane `edge_coverage_linear(p, a, b, inside)` is
// nonzero on, widened by EXTENT_SLACK. Returns false for a zero-length
// edge, whose coverage (and so the primitive's) is 0 everywhere.
bool clip_edge(vec2 a, vec2 b, float inside) {
    vec2 d = b - a;
    float len = length(d);
    if (len < 1e-6) {
        return false;
    }
    clip_half_plane(a, vec2(-d.y, d.x) / len * inside, 0.5 + EXTENT_SLACK);
    return true;
}

void start_poly(vec2 lo, vec2 hi) {
    poly[0] = lo;
    poly[1] = vec2(hi.x, lo.y);
    poly[2] = hi;
    poly[3] = vec2(lo.x, hi.y);
    poly_n = 4;
}

// Mask-local quad corner for `quad` in {0,1}^2 over the clipped
// polygon's bbox, rounded outward a pixel and clamped to the draw bbox;
// a zero-area quad when nothing is left.
vec2 poly_quad_corner(bool live, vec2 quad) {
    if (!live || poly_n == 0) {
        return vec2(0.0);
    }
    vec2 lo = poly[0];
    vec2 hi = poly[0];
    for (int i = 1; i < poly_n; i++) {
        lo = min(lo, poly[i]);
        hi = max(hi, poly[i]);
    }
    lo = clamp(floor(lo - pc.bbox_origin_pixel) - 1.0, vec2(0.0), pc.bbox_size_pixel);
    hi = clamp(ceil(hi - pc.bbox_origin_pixel) + 1.0, vec2(0.0), pc.bbox_size_pixel);
    return mix(lo, hi, quad);
}

void main() {
    // Unit-quad index pattern: (0,0), (1,0), (0,1), (1,1) for
    // TRIANGLE_STRIP. The vertex shader is invoked 4 times per
    // instance (gl_VertexIndex in [0..4)) and emits the four
    // corners of this instance's extent in NDC.
    //
    // Same convention as the trapezoid pipeline: the quad emits at
    // MaskScratch-LOCAL coords (0..bbox_w, 0..bbox_h), not absolute
    // mask coords. The fragment shader translates back to absolute
    // coords by adding `bbox_origin_pixel` to `gl_FragCoord` for the
    // edge math (triangle corners arrive in absolute pixel coords
    // from the X protocol).
    vec2 quad = vec2(float(gl_VertexIndex & 1),
                     float((gl_VertexIndex >> 1) & 1));
    p1 = in_p1;
    p2 = in_p2;
    p3 = in_p3;

    // Signed area × 2 of (p1, p2, p3). Positive ⇒ CCW; negative ⇒ CW;
    // |area_2| below the epsilon ⇒ collinear (degenerate); the
    // fragment shader discards on `orient == 0.0`.
    //
    // Sign convention (see header doc): CCW interior has positive
    // signed_dist against each edge; we want signed_dist * inside_side
    // to be NEGATIVE for interior so the coverage formula returns
    // high values. Hence CCW → orient = -1, CW → orient = +1.
    float signed_area_2 =
        (in_p2.x - in_p1.x) * (in_p3.y - in_p1.y) -
        (in_p2.y - in_p1.y) * (in_p3.x - in_p1.x);
    // Epsilon is in pixel² units. 1e-3 covers floating-point noise
    // around collinear configurations while allowing any visibly
    // non-degenerate triangle through (~0.03 px on a side).
    float area_eps = 1e-3;
    if (abs(signed_area_2) < area_eps) {
        orient = 0.0;
    } else if (signed_area_2 > 0.0) {
        orient = -1.0;
    } else {
        orient = 1.0;
    }

    start_poly(pc.bbox_origin_pixel, pc.bbox_origin_pixel + pc.bbox_size_pixel);
    bool live = orient != 0.0;
    live = live && clip_edge(in_p1, in_p2, orient);
    live = live && clip_edge(in_p2, in_p3, orient);
    live = live && clip_edge(in_p3, in_p1, orient);
    vec2 pixel = poly_quad_corner(live, quad);
    vec2 ndc = pixel / pc.mask_extent * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
