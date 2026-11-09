#version 450

// One instance supplies an integer-aligned destination rectangle. The four
// strip vertices are generated from gl_VertexIndex, as in logic_fill.vert.
layout(location = 0) in vec2 dst_origin;
layout(location = 1) in vec2 dst_size;

layout(push_constant) uniform PushConsts {
    vec2 dst_origin_unused;
    vec2 dst_size_unused;
    vec2 viewport;
    vec2 _pad;
    vec4 fg_color;
} pc;

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 dst_pixel = dst_origin + quad * dst_size;
    // Move the bottom-left corner 1/16 px right. Coverage is unchanged for
    // integer-aligned rectangles: the left edge stays left of x + 1/2 at
    // every row, so no pixel centre enters or leaves the rectangle. The
    // shape is no longer an axis-aligned rectangle, which keeps llvmpipe
    // (lavapipe) off its linear rasterizer path. That path reads a push
    // constant at a nonzero offset wrongly and wrote the wrong colour for a
    // full 1x1 destination (Mesa 26.2.5):
    // https://gitlab.freedesktop.org/mesa/mesa/-/work_items/16828
    dst_pixel.x += quad.y * (1.0 - quad.x) * (1.0 / 16.0);
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
