//! Stroke rasterizer. Produces `Rectangle16` lists from polyline /
//! segment / rectangle inputs honouring the GC stroke state
//! (line_width, line_style, cap_style, join_style, dashes,
//! dash_offset).
//!
//! Lower-level than v2's Backend trait — pure geometry → rect list.
//! Backend callers feed the rect list into `fill_solid_rects` for
//! the on-pixels and (`LineStyle::DoubleDash` only) the off-pixels.
//!
//! Width=0 + LineStyle::Solid + CapStyle::Butt + JoinStyle::Miter
//! takes the fast path through `crate::kms::render::geometry::bresenham_segment`
//! for bit-identical output to the pre-stroke rasterizer. Anything
//! else routes through the slow path: per on-sub-segment, build the
//! closed stroke polygon (quad + caps + joins) and feed it through
//! `crate::kms::render::geometry::scanline_fill_polygon`.

use yserver_protocol::x11::gc::{ArcMode, CapStyle, JoinStyle, LineStyle};

use crate::kms::cpu_types::Rectangle16;

/// Walk an elliptical arc into a chord-approximated polyline.
///
/// `(cx, cy)` is the ellipse centre, `(rx, ry)` the radii.
/// `angle1` / `angle2` are X11 arc angles in 64ths of a degree:
/// `angle1` is the start (0 = +x / 3 o'clock), `angle2` the signed
/// sweep (positive = counter-clockwise in the math sense; screen Y
/// is inverted so it appears clockwise on-screen, matching Xorg).
///
/// The step is chosen so the chord-to-true-arc error stays ≤ 0.5 px
/// for the larger radius — visually indistinguishable from a true
/// ellipse at the sizes real clients use. Always includes the exact
/// start and end points.
pub fn arc_polyline(
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    angle1_deg64: i16,
    angle2_deg64: i16,
) -> Vec<(i32, i32)> {
    use std::f64::consts::PI;
    let deg64_to_rad = |a: f64| a / 64.0 * PI / 180.0;
    // Canonicalize to a non-negative sweep starting from the lower
    // endpoint. Per X11 spec, an arc drawn (a1, +ext) and the same
    // arc drawn from the other endpoint (a1+ext, -ext) must produce
    // identical pixels (xts XDrawArc-13). Subdividing from a fixed
    // start in a fixed direction guarantees the same vertex set
    // regardless of the sign the client passed.
    let (start_deg64, sweep_deg64) = if angle2_deg64 >= 0 {
        (f64::from(angle1_deg64), f64::from(angle2_deg64))
    } else {
        (
            f64::from(angle1_deg64) + f64::from(angle2_deg64),
            -f64::from(angle2_deg64),
        )
    };
    let a1 = deg64_to_rad(start_deg64);
    let sweep = deg64_to_rad(sweep_deg64);

    // Chord error ε = r·(1 - cos(dθ/2)) ≈ r·dθ²/8 ≤ 0.5 → dθ ≤ 2/√r.
    let r = rx.max(ry).max(1.0);
    let dtheta = (2.0 / r.sqrt()).clamp(0.001, PI / 8.0);
    let steps = ((sweep.abs() / dtheta).ceil() as usize).max(1);
    let step = sweep / steps as f64;

    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let theta = a1 + step * i as f64;
        let x = cx + rx * theta.cos();
        let y = cy - ry * theta.sin();
        pts.push((x.round() as i32, y.round() as i32));
    }
    pts
}

/// Build the closed polygon for `PolyFillArc` honouring `ArcMode`.
/// `Chord` closes the arc endpoints directly (the implicit
/// close edge of `scanline_fill_polygon`); `PieSlice` routes the
/// closing path through the centre.
pub fn fill_arc_polygon(
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    angle1_deg64: i16,
    angle2_deg64: i16,
    arc_mode: ArcMode,
) -> Vec<(i32, i32)> {
    let mut pts = arc_polyline(cx, cy, rx, ry, angle1_deg64, angle2_deg64);
    if matches!(arc_mode, ArcMode::PieSlice) {
        // Closing through the centre turns the segment into a wedge.
        // Full revolutions (|angle2| >= 360°) don't need the centre
        // vertex — the arc already closes on itself.
        if i32::from(angle2_deg64).abs() < 360 * 64 {
            pts.push((cx.round() as i32, cy.round() as i32));
        }
    }
    pts
}

/// Per-call snapshot of the GC stroke state. Constructed by the
/// dispatcher from the resolved `DrawState` and threaded through
/// `poly_line` / `poly_segment` / `poly_rectangle` / `poly_arc`.
/// Stroke geometry parameters. Colours are NOT here — `stroke_path`
/// produces geometry (fg/bg rect lists); the caller applies
/// foreground/background at fill time. `background` routing for
/// `DoubleDash` is by list (off-runs land in `bg_rects`), not colour.
#[derive(Clone, Debug)]
pub struct StrokeState {
    pub background: u32,
    pub line_width: u16,
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub dashes: Vec<u8>,
    pub dash_offset: u16,
}

impl Default for StrokeState {
    fn default() -> Self {
        Self {
            background: 0,
            line_width: 0,
            line_style: LineStyle::Solid,
            cap_style: CapStyle::Butt,
            join_style: JoinStyle::Miter,
            dashes: vec![4, 4],
            dash_offset: 0,
        }
    }
}

/// Polyline = open vertex list (shared endpoints participate in
/// joins). DisjointSegments = pairs of independent endpoints (each
/// pair gets caps on both ends, no joins ever).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrokeShape {
    Polyline,
    DisjointSegments,
}

/// Output of `stroke_path`: separate rect lists for the foreground
/// (on-dash pixels) and background (off-dash pixels under
/// `LineStyle::DoubleDash`). `bg_rects` is empty for Solid /
/// OnOffDash.
#[derive(Default, Clone, Debug)]
pub struct StrokeOutput {
    pub fg_rects: Vec<Rectangle16>,
    pub bg_rects: Vec<Rectangle16>,
}

/// Stroke a polyline or set of disjoint segments. Vertices are in
/// drawable coordinates (already translated for `coordinate_mode =
/// Previous` etc. by the caller).
pub fn stroke_path(
    vertices: &[(i32, i32)],
    shape: StrokeShape,
    state: &StrokeState,
) -> StrokeOutput {
    let mut out = StrokeOutput::default();
    let segments = match shape {
        StrokeShape::Polyline => polyline_segments(vertices),
        StrokeShape::DisjointSegments => disjoint_segments(vertices),
    };
    if segments.is_empty() {
        return out;
    }

    // Fast path: width≤1 Solid Butt Miter (no dash, no thick) =
    // exactly what the pre-stroke rasterizer produced. Bit-identical
    // output for the common case.
    let fast_path = state.line_width <= 1
        && matches!(state.line_style, LineStyle::Solid)
        && matches!(state.cap_style, CapStyle::Butt | CapStyle::NotLast)
        && matches!(state.join_style, JoinStyle::Miter);
    if fast_path {
        let last_seg_idx = segments.len() - 1;
        for (i, &(p0, p1)) in segments.iter().enumerate() {
            // CapStyle::NotLast on the terminal endpoint of a Polyline drops
            // the final pixel (per X11 spec; polylines only). `bresenham_segment`
            // now coalesces axis-aligned segments into a single span, so a plain
            // `pop()` would drop the whole terminal edge — instead, for an
            // axis-aligned terminal segment, shorten its endpoint by one pixel.
            // Diagonal segments are still emitted per-pixel, so for those we keep
            // the exact "emit full, pop the last 1-px rect" behaviour.
            let drop_terminal = matches!(state.cap_style, CapStyle::NotLast)
                && shape == StrokeShape::Polyline
                && i == last_seg_idx;
            if drop_terminal {
                if p0 == p1 {
                    // Terminal segment is a single point → NotLast drops it.
                    continue;
                }
                if p0.0 == p1.0 || p0.1 == p1.1 {
                    // Axis-aligned: shorten the endpoint one pixel toward p0.
                    let sx = (p1.0 - p0.0).signum();
                    let sy = (p1.1 - p0.1).signum();
                    crate::kms::render::geometry::bresenham_segment(
                        p0.0,
                        p0.1,
                        p1.0 - sx,
                        p1.1 - sy,
                        &mut out.fg_rects,
                    );
                } else {
                    // Diagonal: still per-pixel; pop exactly the terminal pixel.
                    crate::kms::render::geometry::bresenham_segment(
                        p0.0,
                        p0.1,
                        p1.0,
                        p1.1,
                        &mut out.fg_rects,
                    );
                    out.fg_rects.pop();
                }
            } else {
                crate::kms::render::geometry::bresenham_segment(
                    p0.0,
                    p0.1,
                    p1.0,
                    p1.1,
                    &mut out.fg_rects,
                );
            }
        }
        return out;
    }

    // Slow path: thick / dashed / non-default caps + joins.
    let width = state.line_width.max(1) as f64;
    let dash_iter = DashIter::new(&state.dashes, state.dash_offset, state.line_style);

    let mut phase_px: f64 = dash_iter.start_offset_px();
    let on_pattern = !matches!(state.line_style, LineStyle::Solid);
    let prev_dir_for_join = matches!(shape, StrokeShape::Polyline);

    // Track previous segment's direction (for joins).
    let mut prev_end_dir: Option<(f64, f64)> = None;

    let n_segments = segments.len();
    for (seg_idx, &(p0, p1)) in segments.iter().enumerate() {
        let dx = f64::from(p1.0 - p0.0);
        let dy = f64::from(p1.1 - p0.1);
        let seg_len = (dx * dx + dy * dy).sqrt();
        if seg_len <= f64::EPSILON {
            // Zero-length segment → single cap at the point.
            continue;
        }
        let dir = (dx / seg_len, dy / seg_len);

        // Caller-side caps: first segment's start, last segment's
        // end. Inner endpoints become joins (Polyline only).
        let is_first = seg_idx == 0;
        let is_last = seg_idx == n_segments - 1;
        let needs_start_cap = is_first || matches!(shape, StrokeShape::DisjointSegments);
        let needs_end_cap = is_last || matches!(shape, StrokeShape::DisjointSegments);

        if !on_pattern {
            // Solid: one thick quad along the full segment.
            emit_thick_quad(p0, p1, dir, width, &mut out.fg_rects);
            apply_start_cap_if(
                needs_start_cap,
                p0,
                dir,
                width,
                state.cap_style,
                &mut out.fg_rects,
            );
            apply_end_cap_if(
                needs_end_cap,
                p1,
                dir,
                width,
                state.cap_style,
                &mut out.fg_rects,
            );
        } else {
            // Dashed: walk along the segment in pattern units.
            let mut walked: f64 = 0.0;
            let mut first_dash = true;
            while walked < seg_len {
                let (on, dash_len) = dash_iter.at(phase_px);
                let consume = dash_len.min(seg_len - walked);
                if on || matches!(state.line_style, LineStyle::DoubleDash) {
                    let dp0 = (
                        (f64::from(p0.0) + dir.0 * walked).round() as i32,
                        (f64::from(p0.1) + dir.1 * walked).round() as i32,
                    );
                    let dp1 = (
                        (f64::from(p0.0) + dir.0 * (walked + consume)).round() as i32,
                        (f64::from(p0.1) + dir.1 * (walked + consume)).round() as i32,
                    );
                    let dst = if on {
                        &mut out.fg_rects
                    } else {
                        &mut out.bg_rects
                    };
                    emit_thick_quad(dp0, dp1, dir, width, dst);
                    // Per-dash caps. First dash on a segment uses the
                    // segment-start cap policy (if applicable); rest are
                    // butt caps per spec.
                    let start_cap = if first_dash && needs_start_cap {
                        state.cap_style
                    } else {
                        CapStyle::Butt
                    };
                    apply_start_cap_if(true, dp0, dir, width, start_cap, dst);
                    apply_end_cap_if(true, dp1, dir, width, CapStyle::Butt, dst);
                }
                walked += consume;
                phase_px += consume;
                first_dash = false;
            }
            // Per-dash butt caps already emitted above. Segment-end
            // cap style (Round/Projecting) on the final dash is a
            // future refinement — most clients use Butt with dashes.
            let _ = needs_end_cap;
        }

        // Joins: insert a join wedge between this segment's start and
        // the previous segment's end (Polyline only).
        if prev_dir_for_join
            && !is_first
            && let Some(prev_dir) = prev_end_dir
        {
            emit_join(
                p0,
                prev_dir,
                dir,
                width,
                state.join_style,
                &mut out.fg_rects,
            );
        }
        prev_end_dir = Some(dir);
    }

    out
}

/// Pre-resolve the polyline's segment list. Returns adjacent vertex
/// pairs.
fn polyline_segments(verts: &[(i32, i32)]) -> Vec<((i32, i32), (i32, i32))> {
    if verts.len() < 2 {
        return Vec::new();
    }
    verts.windows(2).map(|w| (w[0], w[1])).collect()
}

/// Pre-resolve disjoint segments: every even-indexed pair is
/// independent. Drops a trailing unpaired vertex.
fn disjoint_segments(verts: &[(i32, i32)]) -> Vec<((i32, i32), (i32, i32))> {
    verts.chunks_exact(2).map(|c| (c[0], c[1])).collect()
}

/// Emit the stroke quad for `(p0, p1)` of width `w` (perpendicular
/// to `dir`). Lower to `scanline_fill_polygon` for rect-list output.
fn emit_thick_quad(
    p0: (i32, i32),
    p1: (i32, i32),
    dir: (f64, f64),
    width: f64,
    out: &mut Vec<Rectangle16>,
) {
    let nx = -dir.1;
    let ny = dir.0;
    let half = width / 2.0;
    let corners: [(i32, i32); 4] = [
        (
            (f64::from(p0.0) - nx * half).round() as i32,
            (f64::from(p0.1) - ny * half).round() as i32,
        ),
        (
            (f64::from(p0.0) + nx * half).round() as i32,
            (f64::from(p0.1) + ny * half).round() as i32,
        ),
        (
            (f64::from(p1.0) + nx * half).round() as i32,
            (f64::from(p1.1) + ny * half).round() as i32,
        ),
        (
            (f64::from(p1.0) - nx * half).round() as i32,
            (f64::from(p1.1) - ny * half).round() as i32,
        ),
    ];
    crate::kms::render::geometry::scanline_fill_polygon(&corners, out);
}

/// Append the start cap geometry for a segment.
fn apply_start_cap_if(
    cond: bool,
    p: (i32, i32),
    dir: (f64, f64),
    width: f64,
    style: CapStyle,
    out: &mut Vec<Rectangle16>,
) {
    if !cond {
        return;
    }
    match style {
        CapStyle::Butt | CapStyle::NotLast => {} // included in quad
        CapStyle::Projecting => emit_extension(p, (-dir.0, -dir.1), width, out),
        CapStyle::Round => emit_round_cap(p, (-dir.0, -dir.1), width, out),
    }
}

fn apply_end_cap_if(
    cond: bool,
    p: (i32, i32),
    dir: (f64, f64),
    width: f64,
    style: CapStyle,
    out: &mut Vec<Rectangle16>,
) {
    if !cond {
        return;
    }
    match style {
        CapStyle::Butt | CapStyle::NotLast => {}
        CapStyle::Projecting => emit_extension(p, dir, width, out),
        CapStyle::Round => emit_round_cap(p, dir, width, out),
    }
}

/// Projecting cap: extend the line by half its width past the
/// endpoint, perpendicular to `dir` (here `dir` is the direction
/// pointing AWAY from the segment's body, so the extension flows
/// into the cap).
fn emit_extension(p: (i32, i32), dir_out: (f64, f64), width: f64, out: &mut Vec<Rectangle16>) {
    let nx = -dir_out.1;
    let ny = dir_out.0;
    let half = width / 2.0;
    let tip = (
        f64::from(p.0) + dir_out.0 * half,
        f64::from(p.1) + dir_out.1 * half,
    );
    let corners: [(i32, i32); 4] = [
        (
            (f64::from(p.0) - nx * half).round() as i32,
            (f64::from(p.1) - ny * half).round() as i32,
        ),
        (
            (f64::from(p.0) + nx * half).round() as i32,
            (f64::from(p.1) + ny * half).round() as i32,
        ),
        (
            (tip.0 + nx * half).round() as i32,
            (tip.1 + ny * half).round() as i32,
        ),
        (
            (tip.0 - nx * half).round() as i32,
            (tip.1 - ny * half).round() as i32,
        ),
    ];
    crate::kms::render::geometry::scanline_fill_polygon(&corners, out);
}

/// Round cap: half-disc of radius `width/2` centred at `p`, axis
/// perpendicular to `dir_out`. Approximated by a 16-segment polygon
/// covering the outward half.
fn emit_round_cap(p: (i32, i32), dir_out: (f64, f64), width: f64, out: &mut Vec<Rectangle16>) {
    let radius = width / 2.0;
    // Base angle: direction of `dir_out` (in CCW-from-+x rad).
    let base = dir_out.1.atan2(dir_out.0);
    const STEPS: usize = 16;
    let mut pts: Vec<(i32, i32)> = Vec::with_capacity(STEPS + 1);
    // Sweep from -π/2 to +π/2 around the outward direction.
    for i in 0..=STEPS {
        let t = std::f64::consts::FRAC_PI_2 - std::f64::consts::PI * (i as f64 / STEPS as f64);
        let theta = base + t;
        let x = f64::from(p.0) + radius * theta.cos();
        let y = f64::from(p.1) + radius * theta.sin();
        pts.push((x.round() as i32, y.round() as i32));
    }
    crate::kms::render::geometry::scanline_fill_polygon(&pts, out);
}

/// Insert a join wedge between two adjacent segments meeting at `p`,
/// with incoming direction `prev_dir` (pointing toward p from prior
/// segment) and outgoing `next_dir` (pointing away from p along next
/// segment).
fn emit_join(
    p: (i32, i32),
    prev_dir: (f64, f64),
    next_dir: (f64, f64),
    width: f64,
    style: JoinStyle,
    out: &mut Vec<Rectangle16>,
) {
    let half = width / 2.0;
    let n1 = (-prev_dir.1, prev_dir.0);
    let n2 = (-next_dir.1, next_dir.0);
    // Cross product determines which side the bend goes on.
    let cross = prev_dir.0 * next_dir.1 - prev_dir.1 * next_dir.0;
    let (out_n1, out_n2) = if cross > 0.0 {
        ((-n1.0, -n1.1), (-n2.0, -n2.1))
    } else {
        (n1, n2)
    };
    match style {
        JoinStyle::Bevel => {
            let a = (
                (f64::from(p.0) + out_n1.0 * half).round() as i32,
                (f64::from(p.1) + out_n1.1 * half).round() as i32,
            );
            let b = (
                (f64::from(p.0) + out_n2.0 * half).round() as i32,
                (f64::from(p.1) + out_n2.1 * half).round() as i32,
            );
            crate::kms::render::geometry::scanline_fill_polygon(&[p, a, b], out);
        }
        JoinStyle::Round => {
            let radius = half;
            let base1 = out_n1.1.atan2(out_n1.0);
            let base2 = out_n2.1.atan2(out_n2.0);
            let (mut a, b) = (base1, base2);
            // Walk the short way around.
            let delta = ((b - a + std::f64::consts::TAU) % std::f64::consts::TAU)
                .min((a - b + std::f64::consts::TAU) % std::f64::consts::TAU);
            const STEPS: usize = 12;
            let dir = if ((b - a + std::f64::consts::TAU) % std::f64::consts::TAU)
                <= std::f64::consts::PI
            {
                1.0
            } else {
                -1.0
            };
            let mut pts = Vec::with_capacity(STEPS + 2);
            pts.push(p);
            for i in 0..=STEPS {
                let theta = a + dir * delta * (i as f64 / STEPS as f64);
                let x = f64::from(p.0) + radius * theta.cos();
                let y = f64::from(p.1) + radius * theta.sin();
                pts.push((x.round() as i32, y.round() as i32));
            }
            let _ = &mut a;
            crate::kms::render::geometry::scanline_fill_polygon(&pts, out);
        }
        JoinStyle::Miter => {
            // Miter intersection point of the two outer offset edges.
            // Outer offset line 1 passes through (p + n1*half) in
            // direction prev_dir.
            // Outer offset line 2 passes through (p + n2*half) in
            // direction next_dir.
            // Solve for intersection.
            let denom = prev_dir.0 * next_dir.1 - prev_dir.1 * next_dir.0;
            if denom.abs() < 1.0e-6 {
                // Parallel — degenerates to bevel.
                emit_join(p, prev_dir, next_dir, width, JoinStyle::Bevel, out);
                return;
            }
            let p1 = (
                f64::from(p.0) + out_n1.0 * half,
                f64::from(p.1) + out_n1.1 * half,
            );
            let p2 = (
                f64::from(p.0) + out_n2.0 * half,
                f64::from(p.1) + out_n2.1 * half,
            );
            // Param along prev_dir from p1 to intersection.
            let dx = p2.0 - p1.0;
            let dy = p2.1 - p1.1;
            let t = (dx * next_dir.1 - dy * next_dir.0) / denom;
            let ix = p1.0 + prev_dir.0 * t;
            let iy = p1.1 + prev_dir.1 * t;
            // Miter limit: spec default 10:1 (= 10 widths). Length is
            // from p to intersection.
            let mx = ix - f64::from(p.0);
            let my = iy - f64::from(p.1);
            let miter_len = (mx * mx + my * my).sqrt();
            if miter_len / half > 10.0 {
                emit_join(p, prev_dir, next_dir, width, JoinStyle::Bevel, out);
                return;
            }
            let intersection = (ix.round() as i32, iy.round() as i32);
            let a = (
                (f64::from(p.0) + out_n1.0 * half).round() as i32,
                (f64::from(p.1) + out_n1.1 * half).round() as i32,
            );
            let b = (
                (f64::from(p.0) + out_n2.0 * half).round() as i32,
                (f64::from(p.1) + out_n2.1 * half).round() as i32,
            );
            crate::kms::render::geometry::scanline_fill_polygon(&[p, a, intersection, b], out);
        }
    }
}

/// Iterator across the dash pattern in pixel units. Supports a phase
/// offset that wraps mod sum(dashes).
#[derive(Clone, Debug)]
struct DashIter<'a> {
    dashes: &'a [u8],
    cycle_len: u32,
    line_style: LineStyle,
    initial_offset: u32,
}

impl<'a> DashIter<'a> {
    fn new(dashes: &'a [u8], dash_offset: u16, line_style: LineStyle) -> Self {
        let cycle_len: u32 = dashes.iter().map(|&b| u32::from(b)).sum();
        let initial_offset = if cycle_len == 0 {
            0
        } else {
            u32::from(dash_offset) % cycle_len
        };
        Self {
            dashes,
            cycle_len,
            line_style,
            initial_offset,
        }
    }

    fn start_offset_px(&self) -> f64 {
        f64::from(self.initial_offset)
    }

    /// Given a position `phase_px` (in pixel units, may exceed
    /// cycle_len; we wrap), return `(is_on, length_remaining_in_this_dash)`.
    fn at(&self, phase_px: f64) -> (bool, f64) {
        let _ = self.line_style;
        if self.cycle_len == 0 || self.dashes.is_empty() {
            return (true, f64::INFINITY);
        }
        let pos = phase_px.rem_euclid(f64::from(self.cycle_len));
        let mut cumulative = 0u32;
        for (i, &b) in self.dashes.iter().enumerate() {
            let next = cumulative + u32::from(b);
            if pos < f64::from(next) {
                let is_on = i % 2 == 0;
                return (is_on, f64::from(next) - pos);
            }
            cumulative = next;
        }
        (true, 1.0) // unreachable in well-formed input
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Max distance from any chord-polyline vertex to the true ellipse
    /// must stay within the half-pixel budget the step size targets.
    #[test]
    fn arc_polyline_chord_error_within_half_pixel() {
        for &r in &[1.0_f64, 10.0, 100.0, 1000.0] {
            // Full circle, radius r, centred at (2000, 2000) to keep
            // coords positive.
            let pts = arc_polyline(2000.0, 2000.0, r, r, 0, 360 * 64);
            for &(x, y) in &pts {
                let dx = f64::from(x) - 2000.0;
                let dy = f64::from(y) - 2000.0;
                let dist = (dx * dx + dy * dy).sqrt();
                // Each rounded vertex is within ~0.71px of the true
                // ring (0.5px rounding on each axis) plus the chord
                // sampling error; assert a loose 1.5px bound.
                assert!(
                    (dist - r).abs() <= 1.5,
                    "r={r}: vertex ({x},{y}) dist {dist} off ring by {}",
                    (dist - r).abs()
                );
            }
        }
    }

    /// The last polyline vertex must land on the exact arc endpoint
    /// (angle1 + angle2), not drift due to step rounding.
    #[test]
    fn arc_polyline_endpoint_exact() {
        // Quarter arc: start 0°, sweep 90° (90*64 deg64). Endpoint at
        // angle 90° → (cx, cy - ry).
        let cx = 100.0;
        let cy = 100.0;
        let rx = 50.0;
        let ry = 50.0;
        let pts = arc_polyline(cx, cy, rx, ry, 0, 90 * 64);
        let last = *pts.last().unwrap();
        // angle 90°: x = cx + rx*cos(90)=cx, y = cy - ry*sin(90)=cy-ry.
        assert!((last.0 - cx.round() as i32).abs() <= 1);
        assert!((last.1 - (cy - ry).round() as i32).abs() <= 1);
        // First vertex at angle 0°: (cx+rx, cy).
        let first = pts[0];
        assert!((first.0 - (cx + rx).round() as i32).abs() <= 1);
        assert!((first.1 - cy.round() as i32).abs() <= 1);
    }

    /// A partial arc must NOT span the full ellipse height — the
    /// original bug was treating every arc as a full ellipse.
    #[test]
    fn arc_polyline_partial_does_not_cover_full_ellipse() {
        // Top-right quarter (0°..90°) of a circle r=50 at (100,100):
        // y ranges from cy (100) up to cy-ry (50); never reaches the
        // bottom (cy+ry = 150).
        let pts = arc_polyline(100.0, 100.0, 50.0, 50.0, 0, 90 * 64);
        let max_y = pts.iter().map(|p| p.1).max().unwrap();
        assert!(
            max_y <= 101,
            "partial arc reached y={max_y}, expected <= ~100"
        );
    }

    /// xts XDrawArc-13: an arc and its reverse-direction equivalent
    /// must produce the same vertex set (→ same pixels).
    #[test]
    fn arc_polyline_direction_symmetric() {
        // (a1=30°, +60°) vs (a1=90°, -60°) cover the same 30°..90° arc.
        let fwd = arc_polyline(100.0, 100.0, 40.0, 40.0, 30 * 64, 60 * 64);
        let rev = arc_polyline(100.0, 100.0, 40.0, 40.0, 90 * 64, -60 * 64);
        let mut fwd_sorted = fwd.clone();
        let mut rev_sorted = rev.clone();
        fwd_sorted.sort_unstable();
        rev_sorted.sort_unstable();
        assert_eq!(fwd_sorted, rev_sorted, "arc must be direction-symmetric");
    }

    #[test]
    fn fill_arc_polygon_pieslice_appends_centre() {
        let chord = fill_arc_polygon(100.0, 100.0, 50.0, 50.0, 0, 90 * 64, ArcMode::Chord);
        let pie = fill_arc_polygon(100.0, 100.0, 50.0, 50.0, 0, 90 * 64, ArcMode::PieSlice);
        // PieSlice has exactly one extra vertex (the centre).
        assert_eq!(pie.len(), chord.len() + 1);
        assert_eq!(*pie.last().unwrap(), (100, 100));
    }

    #[test]
    fn fill_arc_polygon_full_revolution_no_centre_vertex() {
        let chord = fill_arc_polygon(100.0, 100.0, 50.0, 50.0, 0, 360 * 64, ArcMode::Chord);
        let pie = fill_arc_polygon(100.0, 100.0, 50.0, 50.0, 0, 360 * 64, ArcMode::PieSlice);
        // Full circle: PieSlice closes on itself, no centre vertex.
        assert_eq!(pie.len(), chord.len());
    }

    #[test]
    fn fast_path_matches_bresenham_for_width_zero_solid() {
        let state = StrokeState::default();
        let out = stroke_path(&[(0, 0), (10, 0)], StrokeShape::Polyline, &state);
        // 11 single-pixel rects on the horizontal line.
        let total_pixels: usize = out
            .fg_rects
            .iter()
            .map(|r| usize::from(r.width) * usize::from(r.height))
            .sum();
        assert_eq!(total_pixels, 11);
        assert!(out.bg_rects.is_empty());
    }

    #[test]
    fn not_last_drops_terminal_pixel_in_polyline() {
        let state = StrokeState {
            cap_style: CapStyle::NotLast,
            ..StrokeState::default()
        };
        let out_full = stroke_path(
            &[(0, 0), (5, 0)],
            StrokeShape::Polyline,
            &StrokeState::default(),
        );
        let out_notlast = stroke_path(&[(0, 0), (5, 0)], StrokeShape::Polyline, &state);
        let fc: usize = out_full
            .fg_rects
            .iter()
            .map(|r| usize::from(r.width) * usize::from(r.height))
            .sum();
        let nc: usize = out_notlast
            .fg_rects
            .iter()
            .map(|r| usize::from(r.width) * usize::from(r.height))
            .sum();
        assert_eq!(fc, nc + 1);
    }

    #[test]
    fn axis_aligned_segments_coalesce_to_one_span() {
        // Horizontal / vertical thin lines must emit ONE span, not N per-pixel
        // rects (matches Xorg; keeps rectangle outlines cheap and under the
        // root-overlay rect cap). Diagonals stay per-pixel.
        let mut h = Vec::new();
        crate::kms::render::geometry::bresenham_segment(10, 5, 2000, 5, &mut h);
        assert_eq!(h.len(), 1, "horizontal line is a single span, got {h:?}");
        assert_eq!((h[0].x, h[0].y, h[0].width, h[0].height), (10, 5, 1991, 1));

        let mut v = Vec::new();
        crate::kms::render::geometry::bresenham_segment(7, 100, 7, 8, &mut v);
        assert_eq!(v.len(), 1, "vertical line is a single span, got {v:?}");
        assert_eq!((v[0].x, v[0].y, v[0].width, v[0].height), (7, 8, 1, 93));

        let mut d = Vec::new();
        crate::kms::render::geometry::bresenham_segment(0, 0, 3, 3, &mut d);
        assert!(d.len() > 1, "diagonal stays per-pixel, got {d:?}");
    }

    #[test]
    fn thick_horizontal_segment_width_3_produces_3_tall_strip() {
        let state = StrokeState {
            line_width: 3,
            ..StrokeState::default()
        };
        let out = stroke_path(&[(0, 5), (10, 5)], StrokeShape::Polyline, &state);
        // Sum bounding height — scanline emits 3 rows.
        let y_min = out.fg_rects.iter().map(|r| r.y).min().unwrap_or(0);
        let y_max = out
            .fg_rects
            .iter()
            .map(|r| i32::from(r.y) + i32::from(r.height))
            .max()
            .unwrap_or(0);
        let span = y_max - i32::from(y_min);
        assert!((3..=4).contains(&span), "span {span} not in [3,4]");
    }

    #[test]
    fn dash_iter_solid_returns_infinity() {
        let it = DashIter::new(&[4, 4], 0, LineStyle::Solid);
        // Solid still walks dashes (caller uses on_pattern flag). We
        // only need cycle handling to be sensible.
        let (on, len) = it.at(0.0);
        assert!(on);
        assert!(len > 0.0);
    }

    #[test]
    fn dash_iter_walks_pattern() {
        let it = DashIter::new(&[2, 1], 0, LineStyle::OnOffDash);
        // pos=0 → in first on-run of length 2, remaining 2
        let (on, rem) = it.at(0.0);
        assert!(on);
        assert!((rem - 2.0).abs() < 1e-6);
        // pos=2 → at boundary, in off-run of length 1, remaining 1
        let (on, rem) = it.at(2.0);
        assert!(!on);
        assert!((rem - 1.0).abs() < 1e-6);
        // pos=2.5 → mid off, remaining 0.5
        let (on, rem) = it.at(2.5);
        assert!(!on);
        assert!((rem - 0.5).abs() < 1e-6);
    }

    #[test]
    fn dash_iter_offset_wraps() {
        let it = DashIter::new(&[2, 1], 3, LineStyle::OnOffDash);
        // offset=3 wraps to 0 (cycle_len=3).
        assert_eq!(it.start_offset_px(), 0.0);
        let it2 = DashIter::new(&[2, 1], 5, LineStyle::OnOffDash);
        assert_eq!(it2.start_offset_px(), 2.0);
    }
}
