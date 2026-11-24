//! The per-device coordinate transform behind the XI
//! `Coordinate Transformation Matrix` property.
//!
//! Mirrors `dev->transform` in Xorg (`dix/devices.c:DeviceSetTransform`):
//! the property stores the client-supplied 3×3 matrix verbatim, and the
//! server applies it as a homogeneous transform at use time — the full
//! matrix for absolute positions (applied in normalized coordinates,
//! Xorg's `scale_and_transform`), and the same matrix with only the
//! translation column zeroed for relative deltas (Xorg's
//! `relative_transform`). Both forms divide by the computed `w`, exactly
//! as Xlibre's `pixman_f_transform_point` does in `dix/getevents.c`.

/// Row-major 3×3 homogeneous transform for device coordinates (affine
/// when the last row is `0 0 1`, as the libinput Calibration Matrix
/// always is; the CTM property places no such restriction).
///
/// Elements `m[0..3]` are row 0, `m[3..6]` row 1, `m[6..9]` row 2. The
/// full bottom row participates through perspective division, exactly as
/// Xlibre's `pixman_f_transform_point` does on the `{x, y, 1}` vector in
/// `dix/getevents.c`: a nonzero `w` divides `x` and `y`, a zero `w`
/// leaves the input untouched (the pixman call fails and the original
/// vector is kept).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceTransform {
    /// Row-major matrix elements.
    pub m: [f64; 9],
}

impl DeviceTransform {
    /// The identity matrix — the seeded property value.
    pub const IDENTITY: Self = Self {
        m: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    };

    /// Decode the nine little-endian `f32` elements stored on the wire.
    ///
    /// Caller MUST have validated `data.len() == 36` and that every
    /// element is finite; both are `DeviceSetProperty` checks upstream
    /// (`dix/devices.c` `XI_PROP_TRANSFORM` arm).
    #[must_use]
    pub fn from_f32_data(data: &[u8]) -> Self {
        debug_assert_eq!(data.len(), 36, "CTM wire value is nine f32s");
        let mut m = [0.0f64; 9];
        for (i, chunk) in data.chunks_exact(4).enumerate() {
            m[i] = f64::from(f32::from_le_bytes(chunk.try_into().expect("4-byte chunk")));
        }
        Self { m }
    }

    /// Encode nine little-endian `f32` elements for property storage.
    #[must_use]
    pub fn to_f32_data(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(36);
        for v in self.m {
            out.extend_from_slice(&(v as f32).to_le_bytes());
        }
        out
    }

    /// Whether every stored element is finite — `DeviceSetProperty`'s
    /// `isfinite` gate (`dix/devices.c`, `XI_PROP_TRANSFORM`).
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.m.iter().all(|v| v.is_finite())
    }

    /// Whether this is exactly the identity — lets hot paths skip the
    /// transform without floating-point round-trips.
    #[must_use]
    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }

    /// Transform a relative motion delta. Xorg zeroes only the
    /// translation column for relative motion (`relative_transform` in
    /// `DeviceSetTransform`); the bottom row still contributes to `w` in
    /// the numerator's homogeneous divide. A zero `w` returns the delta
    /// untouched, matching the failed `pixman_f_transform_point` call.
    #[must_use]
    pub fn relative(self, dx: f64, dy: f64) -> (f64, f64) {
        let w = self.m[6] * dx + self.m[7] * dy + self.m[8];
        if w == 0.0 {
            return (dx, dy);
        }
        (
            (self.m[0] * dx + self.m[1] * dy) / w,
            (self.m[3] * dx + self.m[4] * dy) / w,
        )
    }

    /// Transform a normalized absolute coordinate pair (`0..1` over the
    /// device axis range): the full homogeneous application including
    /// translation — Xorg's `pixman_f_transform_point` on the
    /// `scale_from_screen`-normalized point, dividing `x`/`y` by `w`
    /// (a zero `w` keeps the input, matching the failed pixman call).
    #[must_use]
    pub fn absolute(self, x: f64, y: f64) -> (f64, f64) {
        let w = self.m[6] * x + self.m[7] * y + self.m[8];
        if w == 0.0 {
            return (x, y);
        }
        (
            (self.m[0] * x + self.m[1] * y + self.m[2]) / w,
            (self.m[3] * x + self.m[4] * y + self.m[5]) / w,
        )
    }
}

impl Default for DeviceTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(m: [f64; 9]) -> DeviceTransform {
        DeviceTransform { m }
    }

    #[test]
    fn identity_leaves_points_and_deltas_alone() {
        let t = DeviceTransform::IDENTITY;
        assert!(t.is_identity());
        assert_eq!(t.relative(3.5, -2.0), (3.5, -2.0));
        assert_eq!(t.absolute(0.25, 0.75), (0.25, 0.75));
    }

    #[test]
    fn relative_drops_translation_but_absolute_keeps_it() {
        // Translate (+10, +5) and scale x by 2.
        let t = transform([2.0, 0.0, 10.0, 0.0, 1.0, 5.0, 0.0, 0.0, 1.0]);
        assert_eq!(t.relative(3.0, 4.0), (6.0, 4.0));
        assert_eq!(t.absolute(0.25, 0.5), (0.25 * 2.0 + 10.0, 0.5 + 5.0));
    }

    #[test]
    fn rotation_applies_to_both_forms() {
        // 90° CCW rotation.
        let t = transform([0.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        let (rx, ry) = t.relative(1.0, 0.0);
        assert!((rx).abs() < f64::EPSILON && (ry - 1.0).abs() < f64::EPSILON);
        let (ax, ay) = t.absolute(1.0, 0.0);
        assert!((ax).abs() < f64::EPSILON && (ay - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn f32_wire_roundtrip() {
        let m = [0.5f32, 0.0, 0.25, -1.0, 2.0, -3.0, 0.0, 0.0, 1.0];
        let mut data = Vec::new();
        for v in m {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let t = DeviceTransform::from_f32_data(&data);
        for (i, v) in m.iter().enumerate() {
            assert_eq!(t.m[i], f64::from(*v));
        }
        let back = t.to_f32_data();
        assert_eq!(back.len(), 36);
        for (i, chunk) in back.chunks_exact(4).enumerate() {
            let got = f32::from_le_bytes(chunk.try_into().unwrap());
            assert_eq!(got, m[i]);
        }
    }

    #[test]
    fn nonfinite_is_detected() {
        let mut t = DeviceTransform::IDENTITY;
        assert!(t.is_finite());
        t.m[4] = f64::NAN;
        assert!(!t.is_finite());
        t.m[4] = f64::INFINITY;
        assert!(!t.is_finite());
    }

    #[test]
    fn homogeneous_scale_matches_normalized_touchpad_matrix() {
        let scaled = transform([130.0, 0.0, 0.0, 0.0, 130.0, 0.0, 0.0, 0.0, 100.0]);
        let normalized = transform([1.3, 0.0, 0.0, 0.0, 1.3, 0.0, 0.0, 0.0, 1.0]);
        assert_eq!(scaled.relative(10.0, 20.0), (13.0, 26.0));
        assert_eq!(scaled.relative(10.0, 20.0), normalized.relative(10.0, 20.0));
        assert_eq!(scaled.absolute(10.0, 20.0), normalized.absolute(10.0, 20.0));
    }

    #[test]
    fn homogeneous_divisor_uses_entire_last_row() {
        let t = transform([2.0, 0.0, 11.0, 0.0, 3.0, -22.0, 0.5, 0.25, 1.0]);
        assert_eq!(t.relative(10.0, 20.0), (20.0 / 11.0, 60.0 / 11.0));
        assert_eq!(t.absolute(10.0, 20.0), (31.0 / 11.0, 38.0 / 11.0));
    }

    #[test]
    fn zero_homogeneous_divisor_preserves_input_like_pixman() {
        let t = transform([2.0, 0.0, 11.0, 0.0, 3.0, -22.0, 1.0, 0.0, -10.0]);
        assert_eq!(t.relative(10.0, 20.0), (10.0, 20.0));
        assert_eq!(t.absolute(10.0, 20.0), (10.0, 20.0));
    }
}
