#version 450

// Solid-fill fragment shader (sub-phase 4.1.5). Outputs the
// foreground colour the caller pushed; the pipeline's
// `VkLogicOp` bit-combines it with the existing destination
// pixel.

layout(push_constant) uniform PushConsts {
    vec2 dst_origin;
    vec2 dst_size;
    vec2 viewport;
    vec4 fg_color;
} pc;

layout(location = 0) out vec4 out_color;

void main() {
    out_color = pc.fg_color;
}
