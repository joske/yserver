#version 450

// Solid-fill quad with logic op (sub-phase 4.1.5).
// One draw call per rect, 4 vertices via TRIANGLE_STRIP. Push
// constants give the destination rect in mirror pixel coords plus
// the foreground colour the fragment stage writes (subject to the
// pipeline's enabled `VkLogicOp`).

layout(push_constant) uniform PushConsts {
    vec2 dst_origin;
    vec2 dst_size;
    vec2 viewport;
    vec4 fg_color;
} pc;

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 dst_pixel = pc.dst_origin + quad * pc.dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
