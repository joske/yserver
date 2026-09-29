//! RANDR CRTC transforms (`RRSetCrtcTransform`, Xorg randr/rrtransform.c).
//!
//! Design: docs/superpowers/specs/2026-09-29-randr-crtc-transform-design.md
//! (D1–D3). The matrix is kept exactly as the client sent it (16.16) and
//! maps CRTC scanout pixels to framebuffer (root) pixels.

/// 16.16 `FIXED` one.
pub const FIXED_ONE: i32 = 0x0001_0000;

/// The identity matrix as `RRTransformInit` stores it.
pub const IDENTITY_MATRIX: [i32; 9] = [FIXED_ONE, 0, 0, 0, FIXED_ONE, 0, 0, 0, FIXED_ONE];

/// A Render filter as `PictureSetDefaultFilters` registers them
/// (render/filter.c:245-265): aliases resolve to the canonical filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Nearest,
    Bilinear,
    Convolution,
}

impl Filter {
    /// `PictureFindFilter`: ISO-Latin-1 case-insensitive, and the name
    /// ends at the first NUL (`CompareISOLatin1Lowered`).
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let name = &name[..end];
        let is = |candidate: &str| name.eq_ignore_ascii_case(candidate.as_bytes());
        if is("nearest") || is("fast") {
            Some(Self::Nearest)
        } else if is("bilinear") || is("good") || is("best") {
            Some(Self::Bilinear)
        } else if is("convolution") {
            Some(Self::Convolution)
        } else {
            None
        }
    }

    /// The name `GetCrtcTransform` returns (`filter->name`).
    #[must_use]
    pub fn canonical_name(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Bilinear => "bilinear",
            Self::Convolution => "convolution",
        }
    }

    /// The filter's `ValidateParams`; nearest and bilinear have none and
    /// accept any parameters.
    #[must_use]
    pub fn params_valid(self, params: &[i32]) -> bool {
        match self {
            Self::Nearest | Self::Bilinear => true,
            Self::Convolution => convolution_params_valid(params),
        }
    }
}

/// `convolutionFilterValidateParams` (render/filter.c:219-242).
fn convolution_params_valid(params: &[i32]) -> bool {
    if params.len() < 3 {
        return false;
    }
    if params[0] & 0xffff != 0 || params[1] & 0xffff != 0 {
        return false;
    }
    let w = i64::from(params[0] >> 16);
    let h = i64::from(params[1] >> 16);
    let remaining = i64::try_from(params.len() - 2).unwrap_or(i64::MAX);
    w * h <= remaining
}

/// One CRTC transform (`RRTransformRec`).
#[derive(Debug, Clone, PartialEq)]
pub struct CrtcTransform {
    /// Row-major 16.16 matrix as received.
    pub matrix: [i32; 9],
    /// `matrix` as doubles (`pixman_f_transform_from_pixman_transform`).
    pub forward: [f64; 9],
    /// `pixman_f_transform_invert` of `forward`.
    pub inverse: [f64; 9],
    pub filter: Option<Filter>,
    pub params: Vec<i32>,
}

impl Default for CrtcTransform {
    fn default() -> Self {
        Self::identity()
    }
}

impl CrtcTransform {
    /// `RRTransformInit`: identity, no filter.
    #[must_use]
    pub fn identity() -> Self {
        let forward = fixed_to_double(&IDENTITY_MATRIX);
        Self {
            matrix: IDENTITY_MATRIX,
            forward,
            inverse: forward,
            filter: None,
            params: Vec::new(),
        }
    }

    /// A transform for `matrix`, or `None` when it is not invertible
    /// (`ProcRRSetCrtcTransform` BadMatch).
    #[must_use]
    pub fn new(matrix: [i32; 9], filter: Option<Filter>, params: Vec<i32>) -> Option<Self> {
        let forward = fixed_to_double(&matrix);
        let inverse = invert(&forward)?;
        Some(Self {
            matrix,
            forward,
            inverse,
            filter,
            params,
        })
    }

    /// Whether `matrix` has an inverse (`pixman_f_transform_invert`).
    #[must_use]
    pub fn invertible(matrix: &[i32; 9]) -> bool {
        invert(&fixed_to_double(matrix)).is_some()
    }

    /// `pixman_transform_is_identity`: equal diagonal (within 2 units),
    /// zero elsewhere, so a uniform homogeneous scale also counts.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        matrix_is_identity(&self.matrix)
    }

    /// D2's accepted form: `m11 > 0`, `m22 > 0`, `m33 = 1`, all else 0.
    #[must_use]
    pub fn is_pure_scale(&self) -> bool {
        let m = &self.matrix;
        m[0] > 0 && m[4] > 0 && m[8] == FIXED_ONE && [m[1], m[2], m[3], m[5], m[6], m[7]] == [0; 6]
    }

    /// `RRTransformEqual`: identities compare equal whatever their filter.
    #[must_use]
    pub fn equivalent(&self, other: &Self) -> bool {
        match (self.is_identity(), other.is_identity()) {
            (true, true) => true,
            (false, false) => {
                self.matrix == other.matrix
                    && self.filter == other.filter
                    && self.params == other.params
            }
            _ => false,
        }
    }

    /// `RRTransformCopy`: what becomes current; an identity drops its filter.
    #[must_use]
    pub fn applied(&self) -> Self {
        if self.is_identity() {
            Self::identity()
        } else {
            self.clone()
        }
    }

    /// Framebuffer footprint of a `mode_w`×`mode_h` mode:
    /// `RRModeGetScanoutSize` (rrcrtc.c:1030-1048) over the fixed matrix.
    #[must_use]
    pub fn footprint(&self, mode_w: u16, mode_h: u16) -> (u16, u16) {
        if self.is_identity() {
            return (mode_w, mode_h);
        }
        // pixman_box16 is int16: the mode edge is stored as Xorg does.
        let mut bbox = Box16 {
            x1: 0,
            y1: 0,
            x2: mode_w as i16,
            y2: mode_h as i16,
        };
        transform_bounds(&self.matrix, &mut bbox);
        // `rep.width = width` narrows int to CARD16.
        (
            (i32::from(bbox.x2) - i32::from(bbox.x1)) as u16,
            (i32::from(bbox.y2) - i32::from(bbox.y1)) as u16,
        )
    }
}

fn matrix_is_identity(m: &[i32; 9]) -> bool {
    let within = |a: i32, b: i32| (i64::from(a) - i64::from(b)).abs() <= 2;
    within(m[0], m[4])
        && within(m[0], m[8])
        && !within(m[0], 0)
        && [m[1], m[2], m[3], m[5], m[6], m[7]]
            .iter()
            .all(|&v| within(v, 0))
}

fn fixed_to_double(m: &[i32; 9]) -> [f64; 9] {
    m.map(|v| f64::from(v) / 65536.0)
}

/// `pixman_f_transform_invert`, row-major.
fn invert(src: &[f64; 9]) -> Option<[f64; 9]> {
    const A: [usize; 3] = [2, 2, 1];
    const B: [usize; 3] = [1, 0, 0];
    let m = |r: usize, c: usize| src[r * 3 + c];
    let mut det = 0.0;
    for i in 0..3 {
        let (ai, bi) = (A[i], B[i]);
        let mut p = m(i, 0) * (m(ai, 2) * m(bi, 1) - m(ai, 1) * m(bi, 2));
        if i == 1 {
            p = -p;
        }
        det += p;
    }
    if det == 0.0 {
        return None;
    }
    let det = 1.0 / det;
    let mut dst = [0.0; 9];
    for j in 0..3 {
        for i in 0..3 {
            let (ai, aj, bi, bj) = (A[i], A[j], B[i], B[j]);
            let mut p = m(ai, aj) * m(bi, bj) - m(ai, bj) * m(bi, aj);
            if (i + j) & 1 != 0 {
                p = -p;
            }
            dst[j * 3 + i] = det * p;
        }
    }
    Some(dst)
}

struct Box16 {
    x1: i16,
    y1: i16,
    x2: i16,
    y2: i16,
}

/// `pixman_transform_bounds`: the four corners through
/// `pixman_transform_point`, floor/ceil outward. A corner that does not
/// transform stops the walk with the box as far as it got, as pixman does.
fn transform_bounds(matrix: &[i32; 9], b: &mut Box16) {
    let corners = [(b.x1, b.y1), (b.x2, b.y1), (b.x2, b.y2), (b.x1, b.y2)];
    for (i, (x, y)) in corners.into_iter().enumerate() {
        let Some((vx, vy)) = transform_point(matrix, i32::from(x), i32::from(y)) else {
            return;
        };
        let floor = |v: i32| (v >> 16) as i16;
        let ceil = |v: i32| (v.wrapping_add(0xffff) >> 16) as i16;
        let (x1, y1, x2, y2) = (floor(vx), floor(vy), ceil(vx), ceil(vy));
        if i == 0 {
            *b = Box16 { x1, y1, x2, y2 };
        } else {
            b.x1 = b.x1.min(x1);
            b.y1 = b.y1.min(y1);
            b.x2 = b.x2.max(x2);
            b.y2 = b.y2.max(y2);
        }
    }
}

/// `pixman_transform_point` of the integer point `(x, y)`: 16.16 result,
/// `None` when pixman clamps or the result leaves `pixman_fixed_t`.
fn transform_point(matrix: &[i32; 9], x: i32, y: i32) -> Option<(i32, i32)> {
    let (rx, ry) = transform_point_31_16(
        matrix,
        [i64::from(x) << 16, i64::from(y) << 16, i64::from(FIXED_ONE)],
    )?;
    Some((i32::try_from(rx).ok()?, i32::try_from(ry).ok()?))
}

/// `pixman_transform_point_31_16`: 48.16 in and out, `None` on clamping.
fn transform_point_31_16(m: &[i32; 9], v: [i64; 3]) -> Option<(i64, i64)> {
    let mut tmp = [[0i64; 2]; 3];
    for (i, row) in tmp.iter_mut().enumerate() {
        for (j, &vj) in v.iter().enumerate() {
            let mij = i64::from(m[i * 3 + j]);
            row[0] += mij * (vj >> 16);
            row[1] += mij * (vj & 0xffff);
        }
    }
    let divint = tmp[2][0] + (tmp[2][1] >> 16);
    let divfrac = tmp[2][1] & 0xffff;
    let affine = |row: [i64; 2]| row[0] + ((row[1] + 0x8000) >> 16);
    if divint == i64::from(FIXED_ONE) && divfrac == 0 {
        return Some((affine(tmp[0]), affine(tmp[1])));
    }
    if divint == 0 && divfrac == 0 {
        return None;
    }
    // Projective: numerator and divisor carry 32 fractional bits; pixman
    // keeps the divisor under 48 bits by shifting both.
    let mut hi32 = (divint >> 32) as i32;
    if hi32 < 0 {
        hi32 = !hi32;
    }
    let shift = 32 - hi32.leading_zeros() as i32;
    let scaled = |value: i128, bits: i32| {
        if bits >= 0 {
            value << bits
        } else {
            value >> -bits
        }
    };
    let div = scaled((i128::from(divint) << 16) + i128::from(divfrac), -shift);
    let project = |row: [i64; 2]| {
        let hi = row[0] + (row[1] >> 16);
        let lo = row[1] & 0xffff;
        let num = scaled((i128::from(hi) << 16) + i128::from(lo), 16 - shift);
        i64::try_from(rounded_div(num, div)).ok()
    };
    Some((project(tmp[0])?, project(tmp[1])?))
}

/// `rounded_sdiv_128_by_49`: magnitude rounded half up, then signed.
fn rounded_div(num: i128, div: i128) -> i128 {
    let negative = (num < 0) != (div < 0);
    let (n, d) = (num.unsigned_abs(), div.unsigned_abs());
    let mut q = n / d;
    if (n % d) * 2 >= d {
        q += 1;
    }
    let q = i128::try_from(q).unwrap_or(i128::MAX);
    if negative { -q } else { q }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scale(sx: i32, sy: i32) -> CrtcTransform {
        CrtcTransform::new([sx, 0, 0, 0, sy, 0, 0, 0, FIXED_ONE], None, Vec::new())
            .expect("invertible")
    }

    /// xrandr's `XDoubleToFixed` words for its `--scale` factors.
    const XRANDR_1_6: i32 = 104_857;
    const XRANDR_0_625: i32 = 40_960;
    const XRANDR_0_8: i32 = 52_428;
    const XRANDR_2: i32 = 131_072;
    const XRANDR_0_5: i32 = 32_768;
    const XRANDR_1_333333: i32 = 87_381;
    /// muffin's scale-down 150% word (reads back 1.337494).
    const MUFFIN_1_337494: i32 = 87_654;

    #[test]
    fn footprint_matches_xorg_xrandr_scale_goldens() {
        // spec, "What Xorg does": 1280×800 through `xrandr --scale`.
        for (word, expected) in [
            (XRANDR_1_6, (2048, 1280)),
            (XRANDR_0_625, (800, 500)),
            (XRANDR_0_8, (1024, 640)),
            (XRANDR_2, (2560, 1600)),
            (XRANDR_0_5, (640, 400)),
            (XRANDR_1_333333, (1707, 1067)),
        ] {
            assert_eq!(scale(word, word).footprint(1280, 800), expected, "{word}");
        }
    }

    #[test]
    fn footprint_matches_muffin_screen_sizes() {
        // spec, "What muffin sends": crtc4 at 0,0 and crtc6 at 2560,0,
        // both 2560×1440; screen = max over CRTCs of origin + footprint.
        let extent = |t4: i32, t6: i32| {
            let (w4, h4) = scale(t4, t4).footprint(2560, 1440);
            let (w6, h6) = scale(t6, t6).footprint(2560, 1440);
            (w4.max(2560 + w6), h4.max(h6))
        };
        assert_eq!(extent(FIXED_ONE, XRANDR_2), (7680, 2880), "scale-down 100%");
        assert_eq!(
            extent(FIXED_ONE, XRANDR_1_6),
            (6656, 2304),
            "scale-down 125%"
        );
        assert_eq!(
            extent(FIXED_ONE, MUFFIN_1_337494),
            (5984, 1926),
            "scale-down 150%"
        );
        assert_eq!(
            extent(XRANDR_0_5, XRANDR_0_8),
            (4608, 1152),
            "scale-up 125%"
        );
        assert_eq!(
            scale(XRANDR_0_5, XRANDR_0_5).footprint(2560, 1440),
            (1280, 720)
        );
    }

    #[test]
    fn identity_footprint_is_the_mode() {
        assert_eq!(
            CrtcTransform::identity().footprint(2560, 1440),
            (2560, 1440)
        );
        assert_eq!(
            CrtcTransform::identity().footprint(u16::MAX, 1),
            (u16::MAX, 1)
        );
    }

    #[test]
    fn identity_follows_pixman_is_identity() {
        assert!(CrtcTransform::identity().is_identity());
        // A uniform homogeneous scale is identity to pixman.
        assert!(scale_all(XRANDR_2).is_identity());
        assert!(!scale(XRANDR_2, XRANDR_2).is_identity());
        let mut near = IDENTITY_MATRIX;
        near[0] += 2;
        assert!(matrix_is_identity(&near));
        near[0] += 1;
        assert!(!matrix_is_identity(&near));
    }

    fn scale_all(s: i32) -> CrtcTransform {
        CrtcTransform::new([s, 0, 0, 0, s, 0, 0, 0, s], None, Vec::new()).unwrap()
    }

    #[test]
    fn pure_scale_is_d2s_contract() {
        assert!(scale(XRANDR_1_6, XRANDR_0_8).is_pure_scale());
        assert!(CrtcTransform::identity().is_pure_scale());
        let translate = [
            FIXED_ONE,
            0,
            5 * FIXED_ONE,
            0,
            FIXED_ONE,
            0,
            0,
            0,
            FIXED_ONE,
        ];
        let rotate = [0, -FIXED_ONE, 0, FIXED_ONE, 0, 0, 0, 0, FIXED_ONE];
        let reflect = [-FIXED_ONE, 0, 0, 0, FIXED_ONE, 0, 0, 0, FIXED_ONE];
        let projective = [FIXED_ONE, 0, 0, 0, FIXED_ONE, 0, 1, 0, FIXED_ONE];
        for m in [translate, rotate, reflect, projective] {
            assert!(
                !CrtcTransform::new(m, None, Vec::new())
                    .unwrap()
                    .is_pure_scale()
            );
        }
        assert!(!scale_all(XRANDR_2).is_pure_scale());
    }

    #[test]
    fn singular_matrices_are_not_invertible() {
        assert!(!CrtcTransform::invertible(&[0; 9]));
        assert!(!CrtcTransform::invertible(&[
            FIXED_ONE, 0, 0, 0, 0, 0, 0, 0, FIXED_ONE
        ]));
        assert!(CrtcTransform::invertible(&[
            XRANDR_2, 0, 0, 0, XRANDR_2, 0, 0, 0, FIXED_ONE
        ]));
        let t = scale(XRANDR_2, XRANDR_0_5);
        assert_eq!(t.inverse[0], 0.5);
        assert_eq!(t.inverse[4], 2.0);
        assert_eq!(t.inverse[8], 1.0);
    }

    #[test]
    fn filters_resolve_render_aliases_to_canonical_names() {
        for (name, filter) in [
            (&b"nearest"[..], Filter::Nearest),
            (b"fast", Filter::Nearest),
            (b"bilinear", Filter::Bilinear),
            (b"good", Filter::Bilinear),
            (b"best", Filter::Bilinear),
            (b"convolution", Filter::Convolution),
            (b"GOOD", Filter::Bilinear),
            (b"fast\0junk", Filter::Nearest),
        ] {
            assert_eq!(Filter::from_name(name), Some(filter), "{name:?}");
        }
        assert_eq!(Filter::from_name(b"box"), None);
        assert_eq!(Filter::from_name(b"fastest"), None);
        assert_eq!(Filter::Nearest.canonical_name(), "nearest");
        assert_eq!(Filter::Bilinear.canonical_name(), "bilinear");
    }

    #[test]
    fn only_convolution_validates_parameters() {
        assert!(Filter::Nearest.params_valid(&[1, 2, 3]));
        assert!(Filter::Bilinear.params_valid(&[FIXED_ONE / 2]));
        let (one, two) = (FIXED_ONE, 2 * FIXED_ONE);
        assert!(Filter::Convolution.params_valid(&[one, one, one]));
        assert!(Filter::Convolution.params_valid(&[two, one, one, one]));
        assert!(!Filter::Convolution.params_valid(&[one, one]));
        assert!(!Filter::Convolution.params_valid(&[one + 1, one, one]));
        assert!(!Filter::Convolution.params_valid(&[two, two, one, one, one]));
    }

    #[test]
    fn equivalence_and_apply_follow_rrtransform_equal_and_copy() {
        let filtered_identity =
            CrtcTransform::new(IDENTITY_MATRIX, Some(Filter::Bilinear), vec![1]).unwrap();
        assert!(filtered_identity.equivalent(&CrtcTransform::identity()));
        assert_eq!(filtered_identity.applied(), CrtcTransform::identity());
        let a = CrtcTransform::new(
            [XRANDR_2, 0, 0, 0, XRANDR_2, 0, 0, 0, FIXED_ONE],
            Some(Filter::Bilinear),
            Vec::new(),
        )
        .unwrap();
        let mut b = a.clone();
        assert!(a.equivalent(&b));
        b.filter = Some(Filter::Nearest);
        assert!(!a.equivalent(&b));
        assert!(!a.equivalent(&CrtcTransform::identity()));
        assert_eq!(a.applied(), a);
    }

    #[test]
    fn projective_point_matches_pixman_division() {
        // Uniform homogeneous 2: each coordinate divided back by 2.
        let m = [XRANDR_2, 0, 0, 0, XRANDR_2, 0, 0, 0, XRANDR_2];
        assert_eq!(
            transform_point(&m, 1280, 800),
            Some((1280 << 16, 800 << 16))
        );
        // w = 0 clamps.
        assert_eq!(
            transform_point(&[FIXED_ONE, 0, 0, 0, FIXED_ONE, 0, 0, 0, 0], 1, 1),
            None
        );
    }
}
