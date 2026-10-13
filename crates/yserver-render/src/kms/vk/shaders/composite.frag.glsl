#version 450

// Fragment shader for the composite pass: samples the bound
// texture (one per draw — the source window or pixmap mirror), with
// alpha policy chosen at pipeline-build time via the
// `SRC_ALPHA_MODE` specialization constant.
//
// `SRC_ALPHA_MODE == 0` → force alpha to 1.0 (force-opaque draw —
// today every window-mirror draw lands here until L1 task A.16
// flips the dial).
// `SRC_ALPHA_MODE != 0` → keep sampled alpha (pass-through:
// cursor, alpha pixmaps, and post-A.16 window-mirror draws once
// the per-paint α invariant is established).
//
// Two pipeline variants are built once at backend init; the caller
// picks which one to bind per draw via
// `CompositorPipeline::pipeline_for(use_src_alpha)`. The pipeline
// blend state is src-over either way; the alpha override is what
// differentiates a force-opaque mirror from an alpha pass-through.

layout(constant_id = 0) const int SRC_ALPHA_MODE = 0;

layout(location = 0) in vec2 v_uv;

layout(location = 0) out vec4 out_color;

layout(set = 0, binding = 0) uniform sampler2D tex;

void main() {
    vec4 c = texture(tex, v_uv);
    out_color = (SRC_ALPHA_MODE != 0) ? c : vec4(c.rgb, 1.0);
}
