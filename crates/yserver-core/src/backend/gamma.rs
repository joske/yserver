//! Pure gamma-LUT helpers shared by every backend: the linear identity
//! ramp used to seed a CRTC's cache, and the resample used when a
//! connector lands on a hardware CRTC of a different gamma size.

/// A linear identity ramp of `size` entries: `entry[i] = i * 65535 /
/// (size-1)`, so a fresh CRTC reports neutral gamma (matches Xorg's
/// initial ramp). `size == 0` -> empty; `size == 1` -> `[0]`.
#[must_use]
pub fn identity_ramp(size: u16) -> Vec<u16> {
    let n = usize::from(size);
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![0];
    }
    (0..n)
        .map(|i| u16::try_from(i as u64 * 65535 / (n as u64 - 1)).unwrap_or(u16::MAX))
        .collect()
}

/// Resample one channel from `src.len()` to `dst_len` entries by linear
/// interpolation, preserving endpoints. `dst_len == 1` -> `[src[0]]`;
/// `src.len() == 1` -> `dst_len` copies of `src[0]`; empty `src` -> empty.
#[must_use]
pub fn resample_channel(src: &[u16], dst_len: usize) -> Vec<u16> {
    if src.is_empty() || dst_len == 0 {
        return Vec::new();
    }
    if dst_len == 1 {
        return vec![src[0]];
    }
    if src.len() == 1 {
        return vec![src[0]; dst_len];
    }

    let src_max = src.len() - 1;
    let denom = (dst_len - 1) as u64;
    (0..dst_len)
        .map(|i| {
            // Compute in u64 so arbitrary LUT sizes still stay overflow-safe.
            let num = i as u64 * src_max as u64;
            let lo = (num / denom) as usize;
            let rem = num % denom;
            if rem == 0 || lo >= src_max {
                return src[lo.min(src_max)];
            }
            let a = u64::from(src[lo]);
            let b = u64::from(src[lo + 1]);
            let interp = (a * (denom - rem) + b * rem + denom / 2) / denom;
            u16::try_from(interp).unwrap_or(u16::MAX)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ramp_endpoints_and_size() {
        assert_eq!(identity_ramp(0), Vec::<u16>::new());
        assert_eq!(identity_ramp(1), vec![0]);
        let ramp = identity_ramp(256);
        assert_eq!(ramp.len(), 256);
        assert_eq!(ramp[0], 0);
        assert_eq!(ramp[255], 65535);
        assert_eq!(ramp[128], (128 * 65535 / 255) as u16);
    }

    #[test]
    fn resample_preserves_endpoints_and_handles_degenerate() {
        let src = identity_ramp(256);
        let down = resample_channel(&src, 16);
        assert_eq!(down.len(), 16);
        assert_eq!(down[0], 0);
        assert_eq!(down[15], 65535);
        assert_eq!(resample_channel(&src, 1), vec![0]);
        assert_eq!(resample_channel(&[7], 4), vec![7, 7, 7, 7]);
        assert_eq!(resample_channel(&[], 4), Vec::<u16>::new());
        assert_eq!(resample_channel(&src, 256), src);
    }
}
