#version 450

// RANDR CRTC transform scale pass (spec D4): every mode-local scanout pixel
// d samples the intermediate at i = M · (d + ½). M is the CRTC matrix
// (rotation, reflection and client scale, as RRTransformCompute) without the
// CRTC origin; the intermediate's (0, 0) is the CRTC origin.
//
// Nearest reproduces pixman's fixed-point sample exactly: the 16.16 point
// M · (d + ½) as pixman_transform_point_31_16 rounds it, less
// pixman_fixed_e, floored (pixman-inlines.h). Bilinear uses the linear
// sampler; pixman's 7-bit weights differ by rounding only.

layout(push_constant) uniform PushConsts {
    // Affine rows m11 m12 m13 m21 m22 m23 as 16.16 words.
    int matrix[6];
    // The same rows as floats, for the bilinear path.
    float forward[6];
    // Intermediate extent in pixels.
    vec2 src_size;
    // 1 = nearest, 0 = bilinear.
    uint nearest;
} pc;

layout(set = 0, binding = 0) uniform sampler2D intermediate;

layout(location = 0) out vec4 out_color;

// pixman_fixed_to_int(a·(dx + ½) + b·(dy + ½) + c − pixman_fixed_e) without
// 64-bit ints. pixman sums a·dx + b·dy + c exactly and adds the ½ terms
// rounded: (a·0x8000 + b·0x8000 + 0x8000) >> 16 = (a + b + 1) >> 1. Each
// word splits into floored whole pixels (w >> 16) and a non-negative
// fraction (w & 0xffff), so no product leaves 32 bits.
int nearest_texel(int a, int b, int c, ivec2 d) {
    int h = (a + b + 1) >> 1;
    int whole = (a >> 16) * d.x + (b >> 16) * d.y + (c >> 16) + (h >> 16);
    int frac = (a & 0xffff) * d.x + (b & 0xffff) * d.y + (c & 0xffff) + (h & 0xffff);
    return frac == 0 ? whole - 1 : whole + ((frac - 1) >> 16);
}

void main() {
    ivec2 d = ivec2(gl_FragCoord.xy);
    if (pc.nearest != 0u) {
        ivec2 size = ivec2(pc.src_size);
        ivec2 t = ivec2(nearest_texel(pc.matrix[0], pc.matrix[1], pc.matrix[2], d),
                        nearest_texel(pc.matrix[3], pc.matrix[4], pc.matrix[5], d));
        out_color = texelFetch(intermediate, clamp(t, ivec2(0), size - 1), 0);
    } else {
        vec2 p = vec2(d) + 0.5;
        vec2 i = vec2(pc.forward[0] * p.x + pc.forward[1] * p.y + pc.forward[2],
                      pc.forward[3] * p.x + pc.forward[4] * p.y + pc.forward[5]);
        out_color = texture(intermediate, i / pc.src_size);
    }
}
