#version 450

// Radial diagnostic pattern for copied reverse-PRIME transport. The dominant
// image is a smooth hue wheel whose rays reach the rectangular image edges
// and desaturate into luminance-varying gray. Low coordinate bits plus tiny
// asymmetric edge/corner fiducials make byte/layout corruption diagnosable
// without obscuring the visual pattern.

layout(push_constant) uniform PushConstants {
    uvec2 extent;
    uint frame_token;
    uint _pad;
} pc;

layout(location = 0) out vec4 out_color;

const float PI = 3.14159265358979323846;
const float TAU = 6.28318530717958647692;

vec3 hue_wheel(float hue) {
    vec3 phase = fract(vec3(hue) + vec3(0.0, 2.0 / 3.0, 1.0 / 3.0));
    return clamp(abs(phase * 6.0 - 3.0) - 1.0, 0.0, 1.0);
}

uvec3 edge_code(uvec2 pixel, uint token) {
    uint x = pixel.x;
    uint y = pixel.y;
    if (y == 0u) {
        return uvec3((73u * x + 19u + 11u * token) & 255u,
                     (29u * x + 101u + 7u * token) & 255u,
                     (151u * x + 47u + 13u * token) & 255u);
    }
    if (y + 1u == pc.extent.y) {
        return uvec3((61u * x + 211u + 17u * token) & 255u,
                     (137u * x + 53u + 19u * token) & 255u,
                     (31u * x + 163u + 23u * token) & 255u);
    }
    if (x == 0u) {
        return uvec3((67u * y + 43u + 29u * token) & 255u,
                     (149u * y + 17u + 31u * token) & 255u,
                     (37u * y + 197u + 37u * token) & 255u);
    }
    return uvec3((127u * y + 89u + 41u * token) & 255u,
                 (41u * y + 227u + 43u * token) & 255u,
                 (79u * y + 23u + 47u * token) & 255u);
}

void main() {
    uvec2 pixel = uvec2(gl_FragCoord.xy);
    vec2 half_extent = max(0.5 * vec2(pc.extent), vec2(0.5));
    vec2 centered = vec2(pixel) + 0.5 - half_extent;

    // Negating y makes angle increase counter-clockwise in screen space.
    // The odd-sized-image center has no direction, so pin it instead of
    // invoking the implementation-dependent atan(0, 0) case.
    float angle = dot(centered, centered) == 0.0
                  ? 0.0 : atan(-centered.y, centered.x);
    uint token_byte = pc.frame_token & 255u;
    float token_phase = fract(float(token_byte) * 0.38196601125);
    float hue = fract(angle / TAU + token_phase);
    vec3 wheel = hue_wheel(hue);

    // Chebyshev-normalized distance reaches exactly one on every rectangular
    // edge along a ray, unlike a circular/elliptical length normalization.
    vec2 edge_radius = max(half_extent - vec2(0.5), vec2(0.5));
    float edge_distance = clamp(max(abs(centered.x) / edge_radius.x,
                                    abs(centered.y) / edge_radius.y),
                                0.0, 1.0);
    float angular_ray = 0.5 + 0.5 * cos(24.0 * angle
                                      + float(token_byte) * (PI / 37.0));
    angular_ray = angular_ray * angular_ray * angular_ray;
    float ray_luminance = mix(0.34, 0.70, angular_ray);
    float desaturation = smoothstep(0.18, 1.0, edge_distance);
    vec3 rgb = mix(wheel * mix(0.72, 1.0, angular_ray),
                   vec3(ray_luminance), desaturation);

    // Two low bits per component identify pixel coordinates while changing
    // the visible pattern by at most 3/255 per channel.
    uvec3 quantized = uvec3(round(clamp(rgb, 0.0, 1.0) * 255.0));
    quantized.r = (quantized.r & 252u) | (pixel.x & 3u);
    quantized.g = (quantized.g & 252u) | (pixel.y & 3u);
    quantized.b = (quantized.b & 252u)
                  | ((pixel.x ^ pixel.y ^ pc.frame_token) & 3u);

    // One-pixel coordinate rails detect pitch and orientation errors. Small,
    // deliberately asymmetric, frame-tokenized corner blocks make channel
    // swaps, flips, and stale frames immediately recognizable.
    bool on_edge = pixel.x == 0u || pixel.y == 0u
                   || pixel.x + 1u == pc.extent.x
                   || pixel.y + 1u == pc.extent.y;
    if (on_edge) {
        quantized = edge_code(pixel, token_byte);
    }

    uint marker = max(1u, min(5u, min(pc.extent.x, pc.extent.y) / 2u));
    bool left = pixel.x < marker;
    bool right = pixel.x >= pc.extent.x - marker;
    bool top = pixel.y < marker;
    bool bottom = pixel.y >= pc.extent.y - marker;
    uvec3 marker_token = uvec3(token_byte,
                               (17u * token_byte) & 255u,
                               (31u * token_byte) & 255u);
    if (left && top) {
        quantized = uvec3(241u, 37u, 83u) ^ marker_token;
    } else if (right && top) {
        quantized = uvec3(29u, 211u, 71u) ^ marker_token;
    } else if (left && bottom) {
        quantized = uvec3(47u, 91u, 233u) ^ marker_token;
    } else if (right && bottom) {
        quantized = uvec3(223u, 173u, 19u) ^ marker_token;
    }

    out_color = vec4(vec3(quantized) / 255.0, 1.0);
}
