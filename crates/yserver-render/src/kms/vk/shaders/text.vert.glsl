#version 450

// Batched glyph quad vertex shader (#1 glyph draw batching). One
// `vkCmdDraw(4, N_glyphs, ..)` with TRIANGLE_STRIP topology draws the
// whole run: the 4-vertex quad comes from `gl_VertexIndex`, and each
// glyph's destination rect + atlas rect arrive as per-instance vertex
// attributes (VK_VERTEX_INPUT_RATE_INSTANCE). Per-run constants
// (viewport, atlas extent, foreground) stay in push constants.
//
// Atlas coords are in TEXELS; this shader divides by `atlas_extent`
// so recorded instance data is independent of any later atlas resize.
//
// NDC convention matches `composite.vert.glsl`: y increases
// downward; pixel `(0, 0)` lands at NDC `(-1, -1)` (top-left).
//
// Push-constant layout is plain std430. This block is `vec2, vec2,
// vec4`: the two `vec2`s exactly fill bytes 0..16, so `foreground`
// lands at offset 16 under std430 — identical to what scalar layout
// would give, and matching the host `TextPushConsts` `repr(C)` struct
// (whose compile-time asserts lock size==32 / atlas_extent@8 /
// foreground@16). No `GL_EXT_scalar_block_layout` needed, which lets
// the shader run on devices without the optional `scalarBlockLayout`
// feature (Broadcom V3D / v3dv on the RPi 4/400).
//
// ## The component-alpha coordinate contract
//
// `atlas_wh` is ALWAYS the glyph's LOGICAL size in texels, never the
// packed atlas footprint — a component-alpha glyph occupies
// `4 * logical_w` texels (four adjacent coverage planes: logical R,
// G, B, A) and passing that as `atlas_wh` would stretch one glyph
// across all four planes. The `4w` allocation width never reaches
// either shader stage (design invariant 9).
//
// The fragment shader addresses the planes by `texelFetch` instead,
// which needs three things this stage supplies:
//
//   * `v_local` — the fragment's integer offset INSIDE the glyph,
//     emitted as `quad * dst_size` so it spans `0..w` × `0..h`. The
//     fragment shader recovers the texel index with
//     `ivec2(floor(v_local))`, which is exact because the quad is
//     pixel-aligned (`dst_origin` and `dst_size` are whole pixels),
//     so fragment centres land on `i + 0.5`. Recovering it by
//     multiplying `v_uv` back up by `atlas_extent` instead lands
//     half a texel out at glyph edges — a one-pixel colour fringe on
//     some glyphs and not others.
//   * `v_atlas_origin` — the glyph's atlas top-left in texels, `flat`
//     so no interpolation happens to it. Values are ≤ the 4096 atlas
//     side and therefore exactly representable in f32.
//   * `v_plane_stride` — texels between adjacent packed planes,
//     `flat`. Carried explicitly even though it currently equals
//     `dst_size.x`: that identity holds only because glyph blitting
//     is 1:1, and deriving from it would couple the sampling to an
//     unrelated invariant whose failure mode is a glyph sampling its
//     neighbour's plane. Zero for a single-plane (A8) glyph, where
//     the fragment shader never reads it.

layout(location = 0) in vec2 dst_origin;  // pixels
layout(location = 1) in vec2 dst_size;     // pixels
layout(location = 2) in vec2 atlas_xy;     // texels
layout(location = 3) in vec2 atlas_wh;     // texels — LOGICAL glyph size
layout(location = 4) in uint plane_stride; // texels between planes; 0 for A8

layout(push_constant) uniform PushConsts {
    vec2 viewport;
    vec2 atlas_extent;  // texels
    vec4 foreground;    // RGB used by fragment shader; alpha is 1.0
} pc;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_foreground;
layout(location = 2) out vec2 v_local;
layout(location = 3) flat out ivec2 v_atlas_origin;
layout(location = 4) flat out int v_plane_stride;

void main() {
    vec2 quad = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));

    vec2 dst_pixel = dst_origin + quad * dst_size;
    vec2 ndc = dst_pixel / pc.viewport * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);

    v_uv = (atlas_xy + quad * atlas_wh) / pc.atlas_extent;
    v_foreground = pc.foreground;
    v_local = quad * dst_size;
    v_atlas_origin = ivec2(atlas_xy);
    v_plane_stride = int(plane_stride);
}
