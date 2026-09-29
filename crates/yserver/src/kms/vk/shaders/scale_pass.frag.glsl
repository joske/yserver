#version 450

// RANDR CRTC transform scale pass (spec D4): every mode-local scanout pixel
// d samples the intermediate at i = M · (d + ½). M is a pure scale and no
// CRTC origin takes part; the intermediate's (0, 0) is the CRTC origin.
//
// Nearest reproduces pixman's fixed-point sample exactly: the 16.16 point
// m · (d + ½), less pixman_fixed_e, truncated (pixman-inlines.h). Bilinear
// uses the linear sampler; pixman's 7-bit weights differ by rounding only.

layout(push_constant) uniform PushConsts {
    // M's diagonal as the 16.16 words the client sent.
    uvec2 matrix;
    // The same diagonal as floats, for the bilinear path.
    vec2 scale;
    // Intermediate extent in pixels.
    vec2 src_size;
    // 1 = nearest, 0 = bilinear.
    uint nearest;
} pc;

layout(set = 0, binding = 0) uniform sampler2D intermediate;

layout(location = 0) out vec4 out_color;

// pixman_fixed_to_int(m · (d + ½) − pixman_fixed_e) without 64-bit ints:
// m · d splits into (m >> 16) · d whole pixels plus (m & 0xffff) · d
// 1/65536ths, and m · ½ rounds as pixman_transform_point_31_16 does.
int nearest_texel(uint m, uint d) {
    uint whole = (m >> 16) * d;
    uint frac = (m & 0xffffu) * d + ((m + 1u) >> 1);
    return frac == 0u ? int(whole) - 1 : int(whole + ((frac - 1u) >> 16));
}

void main() {
    uvec2 d = uvec2(gl_FragCoord.xy);
    if (pc.nearest != 0u) {
        ivec2 size = ivec2(pc.src_size);
        ivec2 t = ivec2(nearest_texel(pc.matrix.x, d.x), nearest_texel(pc.matrix.y, d.y));
        out_color = texelFetch(intermediate, clamp(t, ivec2(0), size - 1), 0);
    } else {
        vec2 i = (vec2(d) + 0.5) * pc.scale;
        out_color = texture(intermediate, i / pc.src_size);
    }
}
