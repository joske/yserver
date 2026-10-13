#version 450

// RENDER `Composite` quad vertex shader (sub-phase 4.1.4.6).
// One draw call per request rect, 4 vertices via TRIANGLE_STRIP.
// The fragment stage does the source/mask sampling so it can apply
// per-picture transform + repeat modes; the vertex stage just emits
// the dst-pixel position so the fragment can recover where it is.

layout(push_constant) uniform PushConsts {
    vec2 dst_origin;          // dst pixel origin
    vec2 dst_size;            // dst pixel size
    vec2 viewport;             // dst image extent (for NDC)
    vec2 src_origin;          // source pixel origin (pre-transform)
    vec2 mask_origin;         // mask pixel origin (pre-transform)
    vec2 src_extent;          // source image extent (pixels)
    vec2 mask_extent;         // mask image extent (pixels)
    ivec2 repeat_modes;       // x = src, y = mask (0=None,1=Normal,2=Pad,3=Reflect)
    vec4 src_xform_row0;      // (a, b, tx, _) — affine src transform row 0
    vec4 src_xform_row1;      // (c, d, ty, _) — affine src transform row 1
    vec4 mask_xform_row0;
    vec4 mask_xform_row1;
} pc;

layout(location = 0) out vec2 v_dst_offset;  // dst-pixel offset within the rect

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 dst_pixel = pc.dst_origin + quad * pc.dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
    v_dst_offset = quad * pc.dst_size;
}
