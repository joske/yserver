#version 450

// GPU rasterization for RENDER Trapezoids (gpu-trap T1). Emits a
// quad (4 vertices via TRIANGLE_STRIP) per instance covering the
// pixels that trapezoid can touch, clamped to the per-draw bbox. Per-instance attributes encode the
// trapezoid's geometry; they are flat-interpolated to the fragment
// stage which computes analytic coverage.

layout(push_constant) uniform PushConsts {
    vec2 mask_extent;        // mask scratch image extent (pixels)
    vec2 bbox_origin_pixel;  // top-left of bbox in ABSOLUTE pixel coords
    vec2 bbox_size_pixel;    // bbox size in pixels
    vec2 _pad;
} pc;

// Per-instance trapezoid attributes (stride = 40, INSTANCE rate).
layout(location = 0) in float in_top;
layout(location = 1) in float in_bottom;
layout(location = 2) in vec2 in_left_p1;
layout(location = 3) in vec2 in_left_p2;
layout(location = 4) in vec2 in_right_p1;
layout(location = 5) in vec2 in_right_p2;

layout(location = 0) flat out float top;
layout(location = 1) flat out float bottom;
layout(location = 2) flat out vec2 left_p1;
layout(location = 3) flat out vec2 left_p2;
layout(location = 4) flat out vec2 right_p1;
layout(location = 5) flat out vec2 right_p2;

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
    // gpu-trap T2: the quad emits at MaskScratch-LOCAL coords
    // (0..bbox_w, 0..bbox_h), not absolute mask coords. This puts
    // the GPU-rasterized mask data at MaskScratch[0..bbox_w,
    // 0..bbox_h], matching the pre-gpu-trap CPU-upload convention;
    // the surrounding composite path then samples
    // mask[(dst.x + mask_origin.x)] without changes.
    // The fragment shader still computes coverage in ABSOLUTE coords
    // (trap edges arrive in absolute pixel coords from the X
    // protocol) — it adds `bbox_origin_pixel` to `gl_FragCoord` to
    // recover that.
    vec2 quad = vec2(float(gl_VertexIndex & 1),
                     float((gl_VertexIndex >> 1) & 1));
    // c_top * c_bot is nonzero only for pixel centres in
    // (top - 0.5, bottom + 0.5); the sides narrow that band further.
    vec2 origin = pc.bbox_origin_pixel;
    start_poly(vec2(origin.x, max(origin.y, in_top - 0.5 - EXTENT_SLACK)),
               vec2(origin.x + pc.bbox_size_pixel.x,
                    min(origin.y + pc.bbox_size_pixel.y, in_bottom + 0.5 + EXTENT_SLACK)));
    bool live = in_top - 0.5 - EXTENT_SLACK < in_bottom + 0.5 + EXTENT_SLACK;
    live = live && clip_edge(in_left_p1, in_left_p2, +1.0);
    live = live && clip_edge(in_right_p1, in_right_p2, -1.0);
    vec2 pixel = poly_quad_corner(live, quad);
    vec2 ndc = pixel / pc.mask_extent * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    top = in_top;
    bottom = in_bottom;
    left_p1 = in_left_p1;
    left_p2 = in_left_p2;
    right_p1 = in_right_p1;
    right_p2 = in_right_p2;
}
