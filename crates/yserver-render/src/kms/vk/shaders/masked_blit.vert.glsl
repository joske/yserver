#version 450

// One quad over the dst rect, mapped to absolute dst-pixel coords so
// gl_FragCoord in the fragment stage equals the dst drawable pixel.
layout(push_constant) uniform PushConsts {
    vec2  dst_origin;   // 0   dst rect offset (pixels)
    vec2  dst_size;     // 8   dst rect extent (pixels)
    vec2  viewport;     // 16  dst image extent (pixels) for NDC
    ivec2 copy_offset;  // 24  src_texel = dst_pixel + copy_offset
    ivec2 clip_offset;  // 32  mask_texel = dst_pixel - clip_offset
    ivec2 src_extent;   // 40  src image extent (texels) for OOB discard
    ivec2 mask_extent;  // 48  mask image extent (texels) for OOB discard
} pc;                   // size = 56

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));
    vec2 dst_pixel = pc.dst_origin + quad * pc.dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
