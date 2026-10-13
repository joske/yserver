#version 450

// Quad vertex shader for the per-window composite pass (sub-phase
// 4.1.3.4). Driven by gl_VertexIndex from a 4-vertex
// `vkCmdDraw(4, 1, ...)` with TRIANGLE_STRIP topology — no vertex
// buffer is bound. Push constants carry the destination rect (in
// scanout pixel coords), the viewport size, and the source UV rect
// (already normalised to [0..1] of the source texture).
//
// Output coordinate convention is Vulkan NDC: x in [-1, +1] left to
// right, y in [-1, +1] top to bottom (so y=0 is centre, +1 is
// bottom). Pixel-space `(0, 0)` lands at NDC `(-1, -1)` (top-left).
//
// Source UVs are in [0..1] across the source image; values
// outside [0..1] never occur for the 4.1.3.4 composite (we always
// sample the whole mirror).

layout(push_constant) uniform PushConsts {
    // Destination rect in screen pixels (top-left + size).
    vec2 dst_origin;
    vec2 dst_size;
    // Viewport size in pixels (the scanout output).
    vec2 viewport;
    // Source rect normalised to [0..1] of the bound texture.
    vec2 src_origin;
    vec2 src_size;
} pc;

layout(location = 0) out vec2 v_uv;

void main() {
    // gl_VertexIndex 0..3 → quad corners (0,0) (1,0) (0,1) (1,1).
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));

    vec2 dst_pixel = pc.dst_origin + quad * pc.dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    v_uv = pc.src_origin + quad * pc.src_size;
}
