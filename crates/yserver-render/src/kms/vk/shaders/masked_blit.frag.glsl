#version 450

layout(push_constant) uniform PushConsts {
    vec2  dst_origin;
    vec2  dst_size;
    vec2  viewport;
    ivec2 copy_offset;
    ivec2 clip_offset;
    ivec2 src_extent;
    ivec2 mask_extent;
} pc;

// Binding 0: src image (IDENTITY view). Binding 1: mask image (IDENTITY R8 view).
// texelFetch ignores the sampler, but the descriptor type is COMBINED_IMAGE_SAMPLER.
layout(set = 0, binding = 0) uniform sampler2D src_tex;
layout(set = 0, binding = 1) uniform sampler2D mask_tex;

layout(location = 0) out vec4 out_color;

void main() {
    // gl_FragCoord.xy = dst_pixel + 0.5 (Vulkan, origin top-left). Truncate to the pixel.
    ivec2 dst_pixel = ivec2(gl_FragCoord.xy);

    // Clip mask threshold (1-bit clip, NOT alpha coverage). depth-1 "set" is
    // stored as the R8 byte 0x01, read here as 0x01/255 > 0.0. OOB mask = clipped.
    ivec2 mask_texel = dst_pixel - pc.clip_offset;
    if (mask_texel.x < 0 || mask_texel.y < 0
        || mask_texel.x >= pc.mask_extent.x || mask_texel.y >= pc.mask_extent.y) {
        discard;
    }
    if (texelFetch(mask_tex, mask_texel, 0).r <= 0.0) {
        discard;
    }

    // Project to src; discard OOB-src (never a sampled-zero write).
    ivec2 src_texel = dst_pixel + pc.copy_offset;
    if (src_texel.x < 0 || src_texel.y < 0
        || src_texel.x >= pc.src_extent.x || src_texel.y >= pc.src_extent.y) {
        discard;
    }

    // GXcopy = raw copy. Blend is disabled in the pipeline; write the src texel
    // verbatim. For depth-24 BGRA the X/alpha byte is copied exactly (identity
    // view, no force-opaque). 8-bit UNORM round-trips exactly (gated in Task 10).
    out_color = texelFetch(src_tex, src_texel, 0);
}
