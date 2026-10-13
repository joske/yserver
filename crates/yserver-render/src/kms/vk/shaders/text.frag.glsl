#version 450

// Glyph fragment shader (sub-phase 4.1.4.5). Samples the shared
// R8 glyph atlas (alpha-only) and emits the (premultiplied)
// foreground colour modulated by the sampled coverage — the X11
// RENDER glyph composite where the source is a solid-fill of
// `foreground` and the mask is the glyph bitmap:
// `src IN glyph-coverage`, then the pipeline's per-op blend state
// (`StdPictOp::blend_factors`) applies the PictOp against dst.
//
// Output is premultiplied: `(rgb * cov, a * cov)`. For the common
// opaque foreground (`a == 1.0`) this is byte-identical to the
// historical `(rgb * cov, cov)` Over-only output.
//
// A8_DST — 1 → replicate the computed alpha across all channels so
// an R8_UNORM attachment (a8 mask pixmap — the cairo/Pango
// component-alpha text intermediate) stores alpha in `.r`. Same
// convention as render.frag.glsl's A8_DST constant.
//
// COMPONENT_ALPHA — 1 → the glyph is packed as FOUR horizontally
// adjacent coverage planes in the atlas (logical R, G, B, A, in that
// order, `plane_stride` texels apart) and each colour channel gets
// its OWN coverage: X RENDER component-alpha, i.e. subpixel/LCD text
// AA. The blend equation's per-channel alpha factor rides output
// index 1 (dual-source blending) where the pipeline's `SRC1_*`
// factors pick it up.
//
// The maths below is EXACTLY render.frag.glsl:257-263 with the
// packed coverage standing in for its `mask_sample`. The two shaders
// carry one equation in two places and cannot share code today, so
// they must be read side by side when either changes.

layout(constant_id = 0) const uint A8_DST = 0u;
layout(constant_id = 1) const uint COMPONENT_ALPHA = 0u;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_foreground;
// Component-alpha coordinate inputs. See text.vert.glsl's
// "component-alpha coordinate contract" comment — `v_local` is the
// fragment's offset inside the glyph quad (`0..w` × `0..h`), and the
// two `flat` inputs are the glyph's atlas origin and the packed plane
// stride, both in texels. Unread on the A8 path.
layout(location = 2) in vec2 v_local;
layout(location = 3) flat in ivec2 v_atlas_origin;
layout(location = 4) flat in int v_plane_stride;

// Dual-source output: the SECOND INDEX of location 0, NOT a second
// location — exactly as render.frag.glsl:66 declares it. The
// `SRC1_*` blend factors reference index 1 of attachment 0; a
// `location = 1` output is a second colour attachment and would not
// link against this pipeline's single attachment.
layout(location = 0)            out vec4 out_color;
layout(location = 0, index = 1) out vec4 out_color1;

layout(set = 0, binding = 0) uniform sampler2D atlas;

void main() {
    vec4 fg = v_foreground;

    if (COMPONENT_ALPHA == 1u) {
        // Exact integer texel index inside the glyph. `floor` of the
        // interpolated `quad * dst_size` recovers it because the quad
        // is pixel-aligned, so fragment centres sample at `i + 0.5`.
        ivec2 local = ivec2(floor(v_local));
        // Plane k lives `k * plane_stride` texels to the right of the
        // glyph's atlas origin. Order is LOGICAL R, G, B, A — the
        // upload writes it that way from wire bytes `[B, G, R, A]`
        // (blue is the LOW byte of the little-endian CARD32). A swap
        // here paints plausible text in the wrong colour and errors
        // nothing (design invariant 4).
        vec4 cov;
        cov.r = texelFetch(atlas, v_atlas_origin + ivec2(local.x + 0 * v_plane_stride, local.y), 0).r;
        cov.g = texelFetch(atlas, v_atlas_origin + ivec2(local.x + 1 * v_plane_stride, local.y), 0).r;
        cov.b = texelFetch(atlas, v_atlas_origin + ivec2(local.x + 2 * v_plane_stride, local.y), 0).r;
        // The glyph's OWN alpha, from the fourth plane — the one
        // place the alpha byte is still read, deliberately. Xorg uses
        // `mask.a` for the alpha channel's factor and `mask.rgb` for
        // the colour channels'. It is 255 everywhere on this JVM's
        // glyphs; that is a property of the client, not the format,
        // so it must NOT be assumed to be 1 (design invariant 3).
        cov.a = texelFetch(atlas, v_atlas_origin + ivec2(local.x + 3 * v_plane_stride, local.y), 0).r;

        // render.frag.glsl:262-263, verbatim in substance:
        //   s         = vec4(src.rgb * mask.rgb, src.a * mask.a)
        //   src_alpha = vec4(src.a   * mask.rgb, src.a * mask.a)
        vec4 s = vec4(fg.rgb * cov.rgb, fg.a * cov.a);
        vec4 src_alpha = vec4(fg.a * cov.rgb, fg.a * cov.a);
        // R8 attachments store alpha in `.r`, so both outputs
        // collapse to the alpha channel — render.frag.glsl:305,323
        // does the same for its `A8_DST`.
        out_color = (A8_DST == 1u) ? vec4(s.a) : s;
        out_color1 = (A8_DST == 1u) ? vec4(src_alpha.a) : src_alpha;
    } else {
        float coverage = texture(atlas, v_uv).r;
        float alpha = fg.a * coverage;
        out_color = (A8_DST == 1u) ? vec4(alpha) : vec4(fg.rgb * coverage, alpha);
        // Single-source path: the pipeline's blend factors are the
        // `SRC_ALPHA` family and never reference index 1, but the
        // output is declared unconditionally (as render.frag.glsl
        // does) so one module serves both specializations. Write the
        // uniform src-alpha factor so the value is defined either
        // way.
        out_color1 = vec4(alpha);
    }
}
