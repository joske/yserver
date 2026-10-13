#version 450

// RENDER `Composite` fragment shader (sub-phase 4.1.4.6).
//
// Computes per-pixel UVs from the interpolated dst-pixel offset by
// applying each picture's affine transform (RENDER pictures use 3×3
// matrices; we keep just the affine 2×3 portion since real X11
// clients use affine), then applies the repeat mode in shader-space
// before sampling.
//
// Premultiplied alpha throughout — the source mirror is
// `B8G8R8A8_UNORM` premul; the SolidFill scratch is cleared with
// premultiplied colour by the caller.
//
// Standard PictOps (Clear..Add) blend via fixed-function (MODE=0,
// shader emits the masked source and the pipeline's blend factors
// do the rest). Disjoint/Conjoint PictOps (16..43) use MODE=1 — the
// shader reads the dst pixel from binding 2 and computes
// `s*Fa + d*Fb` directly per the X RENDER spec, with blend disabled
// in the pipeline.

// MODE: 0 = standard fixed-function blend; 1 = manual blend reading
// dst from binding 2 (Disjoint/Conjoint).
layout(constant_id = 0) const int MODE = 0;
// Op code (0..43). Only consulted in MODE=1.
layout(constant_id = 1) const int OP = 0;
// Swizzle the fragment output to replicate alpha across all channels
// when targeting an R8_UNORM (a8 picture) attachment.
layout(constant_id = 2) const int A8_DST = 0;
// COMPONENT_ALPHA: 1 → mask channels act as per-channel alpha for
// the source. The shader emits a second fragment output (location=1,
// the per-channel src alpha) so the pipeline's `SRC1_*` blend
// factors can apply each mask channel independently. Only meaningful
// in MODE=0 (standard ops); MODE=1 with component-alpha is not
// supported yet (cacomposite).
layout(constant_id = 3) const int COMPONENT_ALPHA = 0;
// ALPHA_MODE (L1 plan task A.11): 0 → pass the computed α through to
// the output (depth-32 ARGB destinations, where α is
// client-meaningful). 1 → force α = 1.0 in the BGRA output
// (depth-24 destinations whose α byte is server-owned and must
// land at 0xFF on every painted pixel). Ignored for `A8_DST = 1`
// since R8 attachments store the alpha-replicated `.r`.
layout(constant_id = 4) const int ALPHA_MODE = 0;

layout(push_constant) uniform PushConsts {
    vec2 dst_origin;
    vec2 dst_size;
    vec2 viewport;
    vec2 src_origin;
    vec2 mask_origin;
    vec2 src_extent;
    vec2 mask_extent;
    ivec2 repeat_modes;       // x = src, y = mask
    vec4 src_xform_row0;
    vec4 src_xform_row1;
    vec4 mask_xform_row0;
    vec4 mask_xform_row1;
} pc;

layout(location = 0) in vec2 v_dst_offset;
layout(location = 0) out vec4 out_color;
// Second output for dual-source blending (component_alpha path).
// Pipeline references it via `SRC1_*` blend factors — its `.rgb`
// carries per-channel `src.a * mask.rgb`, the X RENDER per-channel
// alpha factor.
layout(location = 0, index = 1) out vec4 out_color1;

layout(set = 0, binding = 0) uniform sampler2D src_tex;
layout(set = 0, binding = 1) uniform sampler2D mask_tex;
layout(set = 0, binding = 2) uniform sampler2D dst_tex;

const int REPEAT_NONE = 0;
const int REPEAT_NORMAL = 1;
const int REPEAT_PAD = 2;
const int REPEAT_REFLECT = 3;

// Bit layout of `pc.repeat_modes[i]` — see
// `render_pipeline::REPEAT_FORCE_OPAQUE_BIT` on the Rust side.
//   bits 0..=7  — repeat mode constant
//   bit  8      — force_opaque (X11 Render: PictFormat.alpha_mask == 0
//                 ⇒ samples return α = 1.0 regardless of stored bytes)
//   bits 9..=31 — reserved (zero)
const int REPEAT_MODE_MASK = 0xff;
const int REPEAT_FORCE_OPAQUE_BIT = (1 << 8);

// Apply the repeat mode to a UV in `[0,1]` (post-extent normalisation).
// Returns `vec3(uv.xy, 1.0)` if the sample is in-range, or
// `vec3(_, _, 0.0)` if the repeat mode is `None` and the sample falls
// outside `[0, 1]` — caller multiplies the result by `.z` to suppress
// the texel.
vec3 apply_repeat(vec2 uv, int mode) {
    if (mode == REPEAT_NONE) {
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
            return vec3(0.0, 0.0, 0.0);
        }
        return vec3(uv, 1.0);
    } else if (mode == REPEAT_NORMAL) {
        return vec3(uv - floor(uv), 1.0);
    } else if (mode == REPEAT_REFLECT) {
        vec2 t = uv - 2.0 * floor(uv * 0.5);
        // t in [0, 2); reflect [1, 2) back to [0, 1).
        if (t.x >= 1.0) t.x = 2.0 - t.x;
        if (t.y >= 1.0) t.y = 2.0 - t.y;
        return vec3(t, 1.0);
    } else {
        // PAD: clamp.
        return vec3(clamp(uv, vec2(0.0), vec2(1.0)), 1.0);
    }
}

vec3 sample_at(sampler2D tex, vec2 origin_px, vec2 extent_px,
               vec4 row0, vec4 row1, int repeat_mode, vec2 dst_offset) {
    // Pre-transform pixel coordinates: source-relative to `origin_px`
    // plus the dst-pixel offset within the rect.
    vec2 pre = origin_px + dst_offset;
    // Affine: out = M * (pre.x, pre.y, 1)
    float x = row0.x * pre.x + row0.y * pre.y + row0.z;
    float y = row1.x * pre.x + row1.y * pre.y + row1.z;
    // Normalise to UV.
    vec2 uv = vec2(x / extent_px.x, y / extent_px.y);
    return apply_repeat(uv, repeat_mode & REPEAT_MODE_MASK);
}

// X RENDER Disjoint/Conjoint Fa, Fb tables. `aa = src.a`, `ab = dst.a`.
// Denominators that can be zero are guarded — when the matching premul
// value is also zero (which it is, because Cs = 0 when Aa = 0 in
// premul space), the result of multiplying by the unguarded value
// would be NaN/Inf; guarding to 0 keeps it sane.
void disjoint_conjoint_factors(int op, float aa, float ab,
                               out float fa, out float fb) {
    fa = 0.0;
    fb = 0.0;
    if (op == 13) {                                                             // Saturate
        // Per X RENDER (and pixman): treats As as the saturation
        // coefficient. Same shape as DisjointOver but with src
        // saturating against (1-Ad). When Aa = 0 the contribution
        // from src is zero anyway (premul Cs = 0), so any finite
        // Fa works; matching rendercheck/pixman convention by
        // returning 1 in that limit.
        fa = (aa > 0.0) ? min(1.0, (1.0 - ab) / aa) : 1.0;
        fb = 1.0;
    }
    // DisjointClear (16) — fa, fb already 0.
    else if (op == 17) { fa = 1.0; fb = 0.0; }                                  // DisjointSrc
    else if (op == 18) { fa = 0.0; fb = 1.0; }                                  // DisjointDst
    else if (op == 19) {                                                        // DisjointOver
        fa = 1.0;
        fb = (ab > 0.0) ? min(1.0, (1.0 - aa) / ab) : 0.0;
    }
    else if (op == 20) {                                                        // DisjointOverReverse
        fa = (aa > 0.0) ? min(1.0, (1.0 - ab) / aa) : 0.0;
        fb = 1.0;
    }
    else if (op == 21) {                                                        // DisjointIn
        fa = (aa > 0.0) ? max(1.0 - (1.0 - ab) / aa, 0.0) : 0.0;
        fb = 0.0;
    }
    else if (op == 22) {                                                        // DisjointInReverse
        fa = 0.0;
        fb = (ab > 0.0) ? max(1.0 - (1.0 - aa) / ab, 0.0) : 0.0;
    }
    else if (op == 23) {                                                        // DisjointOut
        fa = (aa > 0.0) ? min(1.0, (1.0 - ab) / aa) : 0.0;
        fb = 0.0;
    }
    else if (op == 24) {                                                        // DisjointOutReverse
        fa = 0.0;
        fb = (ab > 0.0) ? min(1.0, (1.0 - aa) / ab) : 0.0;
    }
    else if (op == 25) {                                                        // DisjointAtop
        fa = (aa > 0.0) ? max(1.0 - (1.0 - ab) / aa, 0.0) : 0.0;
        fb = (ab > 0.0) ? min(1.0, (1.0 - aa) / ab) : 0.0;
    }
    else if (op == 26) {                                                        // DisjointAtopReverse
        fa = (aa > 0.0) ? min(1.0, (1.0 - ab) / aa) : 0.0;
        fb = (ab > 0.0) ? max(1.0 - (1.0 - aa) / ab, 0.0) : 0.0;
    }
    else if (op == 27) {                                                        // DisjointXor
        fa = (aa > 0.0) ? min(1.0, (1.0 - ab) / aa) : 0.0;
        fb = (ab > 0.0) ? min(1.0, (1.0 - aa) / ab) : 0.0;
    }
    // ConjointClear (32) — fa, fb already 0.
    else if (op == 33) { fa = 1.0; fb = 0.0; }                                  // ConjointSrc
    else if (op == 34) { fa = 0.0; fb = 1.0; }                                  // ConjointDst
    else if (op == 35) {                                                        // ConjointOver
        fa = 1.0;
        fb = (ab > 0.0) ? max(1.0 - aa / ab, 0.0) : 0.0;
    }
    else if (op == 36) {                                                        // ConjointOverReverse
        fa = (aa > 0.0) ? max(1.0 - ab / aa, 0.0) : 0.0;
        fb = 1.0;
    }
    else if (op == 37) {                                                        // ConjointIn
        fa = (aa > 0.0) ? min(1.0, ab / aa) : 0.0;
        fb = 0.0;
    }
    else if (op == 38) {                                                        // ConjointInReverse
        fa = 0.0;
        fb = (ab > 0.0) ? min(aa / ab, 1.0) : 0.0;
    }
    else if (op == 39) {                                                        // ConjointOut
        fa = (aa > 0.0) ? max(1.0 - ab / aa, 0.0) : 0.0;
        fb = 0.0;
    }
    else if (op == 40) {                                                        // ConjointOutReverse
        fa = 0.0;
        fb = (ab > 0.0) ? max(1.0 - aa / ab, 0.0) : 0.0;
    }
    else if (op == 41) {                                                        // ConjointAtop
        fa = (aa > 0.0) ? min(1.0, ab / aa) : 0.0;
        fb = (ab > 0.0) ? max(1.0 - aa / ab, 0.0) : 0.0;
    }
    else if (op == 42) {                                                        // ConjointAtopReverse
        fa = (aa > 0.0) ? max(1.0 - ab / aa, 0.0) : 0.0;
        fb = (ab > 0.0) ? min(1.0, aa / ab) : 0.0;
    }
    else if (op == 43) {                                                        // ConjointXor
        fa = (aa > 0.0) ? max(1.0 - ab / aa, 0.0) : 0.0;
        fb = (ab > 0.0) ? max(1.0 - aa / ab, 0.0) : 0.0;
    }
}

void main() {
    vec3 src_uv = sample_at(src_tex, pc.src_origin, pc.src_extent,
                            pc.src_xform_row0, pc.src_xform_row1,
                            pc.repeat_modes.x, v_dst_offset);
    vec3 mask_uv = sample_at(mask_tex, pc.mask_origin, pc.mask_extent,
                             pc.mask_xform_row0, pc.mask_xform_row1,
                             pc.repeat_modes.y, v_dst_offset);

    vec4 src = texture(src_tex, src_uv.xy) * src_uv.z;
    vec4 mask_sample = texture(mask_tex, mask_uv.xy) * mask_uv.z;

    // X11 Render: PictFormat.alpha_mask == 0 ⇒ samples must return
    // α = 1.0 regardless of what the storage byte holds. The
    // Rust-side resolver sets this bit on `repeat_modes[i]` when the
    // picture wraps a drawable whose depth is < 32 (the only case
    // where the storage's α byte is server-owned padding). Apply
    // *before* the operator stages — the rest of the shader (premul
    // math, fixed-function blend factors, Disjoint/Conjoint manual
    // blend) assumes the sampled α is the picture-space α, which is
    // exactly the post-fixup value.
    bool src_force_opaque  = (pc.repeat_modes.x & REPEAT_FORCE_OPAQUE_BIT) != 0;
    bool mask_force_opaque = (pc.repeat_modes.y & REPEAT_FORCE_OPAQUE_BIT) != 0;
    if (src_force_opaque) {
        // REPEAT_NONE's out-of-range zero (src_uv.z == 0) must still
        // suppress the texel; only force α to 1.0 *inside* the
        // sampled region — `src_uv.z` is 1.0 there, 0.0 outside.
        src.a = src_uv.z;
    }
    if (mask_force_opaque) {
        mask_sample.a = mask_uv.z;
    }

    vec4 s;          // premul source colour, post-mask
    vec4 src_alpha;  // src alpha factor (per-channel for COMPONENT_ALPHA)
    if (COMPONENT_ALPHA != 0) {
        // Per-channel mask: mask_sample.rgb scales src.rgb; the
        // alpha *factor* used by the blend equation is per-channel
        // src.a * mask.rgb (not mask.a). Output 1 carries this so
        // the pipeline's `SRC1_*` blend factors can use it.
        s = vec4(src.rgb * mask_sample.rgb, src.a * mask_sample.a);
        src_alpha = vec4(src.a * mask_sample.rgb, src.a * mask_sample.a);
    } else {
        // Standard mask: only mask alpha matters; the src alpha
        // factor is uniform across rgb (just src.a).
        float m = mask_sample.a;
        s = src * m;
        src_alpha = vec4(s.a);
    }

    vec4 c;
    if (MODE == 0) {
        c = s;
    } else {
        // Sample dst readback at this fragment's pixel. The readback
        // scratch can be larger than the dst (power-of-two grow) so
        // texelFetch on the integer pixel coordinate is the only
        // reliable way to hit the right texel — UV-based sampling
        // would scale into the unused tail of the scratch.
        ivec2 dst_pixel_int = ivec2(pc.dst_origin + v_dst_offset);
        vec4 d = texelFetch(dst_tex, dst_pixel_int, 0);
        if (COMPONENT_ALPHA != 0) {
            // Per-channel Fa/Fb using src_alpha.X (the per-channel src
            // alpha factor = src.a * mask.X). Each channel is blended
            // with its own coefficients; matches X RENDER's
            // component-alpha + Disjoint/Conjoint semantics.
            float far, fbr, fag, fbg, fab, fbb, faa, fba;
            disjoint_conjoint_factors(OP, src_alpha.r, d.a, far, fbr);
            disjoint_conjoint_factors(OP, src_alpha.g, d.a, fag, fbg);
            disjoint_conjoint_factors(OP, src_alpha.b, d.a, fab, fbb);
            disjoint_conjoint_factors(OP, src_alpha.a, d.a, faa, fba);
            c = vec4(
                s.r * far + d.r * fbr,
                s.g * fag + d.g * fbg,
                s.b * fab + d.b * fbb,
                s.a * faa + d.a * fba
            );
        } else {
            float fa, fb;
            disjoint_conjoint_factors(OP, s.a, d.a, fa, fb);
            c = s * fa + d * fb;
        }
    }
    out_color = (A8_DST != 0) ? vec4(c.a) : c;
    // A.11 originally forced `out_color.a = 1.0` under ALPHA_MODE,
    // but the fragment α here is also the `src.α` factor the
    // pipeline's blend stage uses (ONE_MINUS_SRC_ALPHA on the dst
    // side). Pinning shader α to 1.0 made every transparent source
    // pixel overwrite the destination as opaque black — observed
    // as black borders around desktop icons on first MATE/fuji
    // smoke. The ALPHA_MODE spec-constant is kept declared so the
    // pipeline cache key + plumbing stay intact. The depth-24
    // α-stays-opaque contract is instead enforced OUTSIDE the
    // fragment stage, where it does not disturb the blend factor:
    // `render_pipeline::dst_color_write_mask` drops α from the
    // colour write mask for a no-alpha destination, so the composite
    // never stores an α byte and the storage keeps the opaque value
    // it was initialised with (`default_window_init_color`). Same
    // mechanism `logic_fill_pipeline.rs` uses for core fills.
    // Output 1 is consumed by the pipeline's SRC1_* blend factors
    // when COMPONENT_ALPHA = 1 (MODE=0). For an R8 (a8 picture)
    // attachment the blend reads `.r` of output 1, so we replicate
    // src_alpha's alpha component (= src.a * mask.a) — that's the
    // per-channel alpha factor that matches the picture's single
    // alpha-only channel.
    out_color1 = (A8_DST != 0) ? vec4(src_alpha.a) : src_alpha;
}
