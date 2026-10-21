//! Pure gamma-LUT helpers shared by every backend: the linear identity
//! ramp used to seed a CRTC's cache, the resample used when a connector
//! lands on a hardware CRTC of a different gamma size, and the VidMode
//! gamma table that sits under every CRTC's RANDR ramp.

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

/// Significant bits of the VidMode gamma table, as modesetting asks of
/// `xf86HandleColormaps` (drmmode_display.c): `sigRGBbits` 10.
pub const VIDMODE_GAMMA_BITS: u32 = 10;

/// Entries of the VidMode gamma table, `1 << VIDMODE_GAMMA_BITS`: Xorg's
/// `gammaElements`, which VidMode's `GetGammaRampSize` reports on
/// modesetting whatever the hardware LUT's size.
pub const VIDMODE_GAMMA_ENTRIES: u16 = 1 << VIDMODE_GAMMA_BITS;

/// Entries per channel of the root visual's palette: the depth-24
/// TrueColor visual has 8 bits per channel.
const PALETTE_ENTRIES: usize = 1 << 8;

/// The screen's VidMode gamma table: Xorg's CMap gamma (`xf86cmap.c`)
/// on modesetting, `VIDMODE_GAMMA_ENTRIES` 10-bit values per channel.
/// `SetGamma` builds it from its three values, `SetGammaRamp` from a
/// client's ramp. It sits under each CRTC's RANDR ramp rather than
/// replacing it: [`Self::crtc_lut`] is what the hardware LUT gets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VidModeGammaTable {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}

impl VidModeGammaTable {
    /// The table before any VidMode write: `ComputeGamma` at 1.0.
    #[must_use]
    pub fn identity() -> Self {
        Self::from_gamma([1.0; 3])
    }

    /// `ComputeGamma`: entry `i` is `i` at gamma 1.0, else
    /// `(i / 1023)^(1/gamma) * 1023 + 0.5`, truncated.
    #[must_use]
    pub fn from_gamma(gamma: [f32; 3]) -> Self {
        let channel = |gamma: f32| -> Vec<u16> {
            let top = f64::from(VIDMODE_GAMMA_ENTRIES - 1);
            let exponent = 1.0 / f64::from(gamma);
            (0..VIDMODE_GAMMA_ENTRIES)
                .map(|i| {
                    // Xorg compares the double exactly, and `i` differs from
                    // the power formula's rounding.
                    #[allow(clippy::float_cmp)]
                    if exponent == 1.0 {
                        return i;
                    }
                    let value = (f64::from(i) / top).powf(exponent) * top + 0.5;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let entry = value as u16;
                    entry
                })
                .collect()
        };
        Self {
            red: channel(gamma[0]),
            green: channel(gamma[1]),
            blue: channel(gamma[2]),
        }
    }

    /// `ComputeGammaRamp`: a client's 16-bit ramp of
    /// `VIDMODE_GAMMA_ENTRIES` entries, each kept to its top 10 bits.
    #[must_use]
    pub fn from_ramp(red: &[u16], green: &[u16], blue: &[u16]) -> Self {
        let channel = |ramp: &[u16]| -> Vec<u16> {
            ramp.iter()
                .map(|&value| value >> (16 - VIDMODE_GAMMA_BITS))
                .collect()
        };
        Self {
            red: channel(red),
            green: channel(green),
            blue: channel(blue),
        }
    }

    /// `xf86GetGammaRamp`: the table as a 16-bit ramp, each 10-bit value
    /// shifted up and its top bits repeated below it.
    #[must_use]
    pub fn ramp(&self) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        let channel = |table: &[u16]| -> Vec<u16> {
            table
                .iter()
                .map(|&entry| {
                    let mut value = entry << (16 - VIDMODE_GAMMA_BITS);
                    let mut shift = VIDMODE_GAMMA_BITS;
                    while shift < 16 {
                        value |= value >> shift;
                        shift += VIDMODE_GAMMA_BITS;
                    }
                    value
                })
                .collect()
        };
        (
            channel(&self.red),
            channel(&self.green),
            channel(&self.blue),
        )
    }

    /// A CRTC's hardware LUT of `size` entries for its RANDR ramp, as
    /// modesetting loads it: the root TrueColor palette
    /// (`CMapRefreshColors`, entry `i` the table's `i * 1023 / 255`), each
    /// palette value looked up in the RANDR ramp, then spread over or
    /// sampled down to the hardware size (`xf86RandR12CrtcComputeGamma`).
    ///
    /// Xorg's RANDR ramp has the table's 1024 entries, so a palette value
    /// indexes it directly. Yserver's follows the hardware LUT's size, so
    /// the value is scaled to it; at 1024 entries that is the same index.
    /// An empty RANDR ramp is Xorg's absent one: the value shifted to 16
    /// bits.
    #[must_use]
    pub fn crtc_lut(
        &self,
        randr: (&[u16], &[u16], &[u16]),
        size: usize,
    ) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        (
            crtc_channel(&self.red, randr.0, size),
            crtc_channel(&self.green, randr.1, size),
            crtc_channel(&self.blue, randr.2, size),
        )
    }
}

/// One channel of [`VidModeGammaTable::crtc_lut`].
fn crtc_channel(table: &[u16], randr: &[u16], size: usize) -> Vec<u16> {
    let top = usize::from(VIDMODE_GAMMA_ENTRIES - 1);
    let palette: Vec<u16> = (0..PALETTE_ENTRIES)
        .map(|i| {
            table
                .get(i * top / (PALETTE_ENTRIES - 1))
                .copied()
                .unwrap_or(0)
        })
        .collect();
    let lookup = |value: u16| -> u16 {
        if randr.is_empty() {
            return value << (16 - VIDMODE_GAMMA_BITS);
        }
        let index = if randr.len() == usize::from(VIDMODE_GAMMA_ENTRIES) {
            usize::from(value)
        } else {
            usize::from(value) * (randr.len() - 1) / top
        };
        randr[index.min(randr.len() - 1)]
    };
    if size == 0 {
        return Vec::new();
    }
    if size >= PALETTE_ENTRIES {
        // Upsampling: each palette entry fills `size / 256` slots, and the
        // last one the rest of a size that is not a multiple.
        let slots = size / PALETTE_ENTRIES;
        let mut lut = Vec::with_capacity(size);
        for &value in &palette {
            lut.extend(std::iter::repeat_n(lookup(value), slots));
        }
        let last = lookup(palette[PALETTE_ENTRIES - 1]);
        lut.resize(size, last);
        lut
    } else {
        // Downsampling; a one-entry LUT takes the first palette entry.
        let span = (size - 1).max(1);
        (0..size)
            .map(|i| lookup(palette[i * (PALETTE_ENTRIES - 1) / span]))
            .collect()
    }
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

    /// Xorg's RANDR ramp for a gamma, `init_one_component`
    /// (xf86RandR12.c): the CRTC ramps the expected values below were
    /// computed against.
    fn xorg_randr_ramp(size: u32, gamma: f64) -> Vec<u16> {
        let mut shift = 0;
        while (size << shift) < (1 << 16) {
            shift += 1;
        }
        let top = f64::from(size - 1);
        (0..size)
            .map(|i| {
                if (gamma - 1.0).abs() < f64::EPSILON {
                    (i << shift) as u16
                } else {
                    ((f64::from(i) / top).powf(1.0 / gamma) * top * f64::from(1u32 << shift)) as u16
                }
            })
            .collect()
    }

    fn at(lut: &(Vec<u16>, Vec<u16>, Vec<u16>), i: usize) -> (u16, u16, u16) {
        (lut.0[i], lut.1[i], lut.2[i])
    }

    /// Every expected value here was printed by Xorg's own `ComputeGamma`,
    /// `ComputeGammaRamp`, `xf86GetGammaRamp` and
    /// `xf86RandR12CrtcComputeGamma`, extracted from the xserver source
    /// and compiled unchanged, over the TrueColor palette
    /// `CMapRefreshColors` builds for a depth-24 root, against RANDR
    /// ramps of 1024 entries at gamma 1.0, 1.25 and 1.0.
    #[test]
    fn vidmode_gamma_table_matches_xorgs_modesetting_path() {
        let identity = VidModeGammaTable::identity();
        let ramp = identity.ramp();
        assert_eq!(
            (ramp.0[0], ramp.0[1], ramp.0[512], ramp.0[1023]),
            (0, 64, 32800, 65535)
        );

        let randr_red = xorg_randr_ramp(1024, 1.0);
        let randr_green = xorg_randr_ramp(1024, 1.25);
        let randr = (
            randr_red.as_slice(),
            randr_green.as_slice(),
            randr_red.as_slice(),
        );

        let table = VidModeGammaTable::from_gamma([2.0, 1.0, 0.5]);
        assert_eq!((table.red[1], table.green[1], table.blue[1]), (32, 1, 0));
        assert_eq!(
            (table.red[512], table.green[512], table.blue[512]),
            (724, 512, 256)
        );
        let lut = table.crtc_lut(randr, 256);
        assert_eq!(at(&lut, 0), (0, 0, 0));
        assert_eq!(at(&lut, 1), (4096, 775, 0));
        assert_eq!(at(&lut, 2), (5760, 1350, 0));
        assert_eq!(at(&lut, 100), (40960, 30950, 10048));
        assert_eq!(at(&lut, 128), (46336, 37691, 16448));
        assert_eq!(at(&lut, 255), (65472, 65472, 65472));
        let lut = table.crtc_lut(randr, 1024);
        assert_eq!(at(&lut, 3), (0, 0, 0));
        assert_eq!(at(&lut, 4), (4096, 775, 0));
        assert_eq!(at(&lut, 7), (4096, 775, 0));
        assert_eq!(at(&lut, 512), (46336, 37691, 16448));
        assert_eq!(at(&lut, 1023), (65472, 65472, 65472));
        let lut = table.crtc_lut(randr, 4096);
        assert_eq!(at(&lut, 15), (0, 0, 0));
        assert_eq!(at(&lut, 16), (4096, 775, 0));
        assert_eq!(at(&lut, 2048), (46336, 37691, 16448));
        assert_eq!(at(&lut, 4095), (65472, 65472, 65472));
        let lut = table.crtc_lut(randr, 100);
        assert_eq!(at(&lut, 1), (5760, 1350, 0));
        assert_eq!(at(&lut, 50), (46336, 37691, 16448));
        assert_eq!(at(&lut, 99), (65472, 65472, 65472));

        let red: Vec<u16> = (0..1024).map(|i| 65535 - i * 64).collect();
        let green: Vec<u16> = (0..1024).map(|i| i * 64 + 63).collect();
        let blue: Vec<u16> = (0..1024u32).map(|i| (i * 37) as u16).collect();
        let table = VidModeGammaTable::from_ramp(&red, &green, &blue);
        assert_eq!((table.red[0], table.green[0], table.blue[0]), (1023, 0, 0));
        assert_eq!(
            (table.red[1023], table.green[1023], table.blue[1023]),
            (0, 1023, 591)
        );
        let ramp = table.ramp();
        assert_eq!(at(&ramp, 0), (65535, 0, 0));
        assert_eq!(at(&ramp, 1), (65471, 64, 0));
        assert_eq!(at(&ramp, 1023), (0, 65535, 37860));
        let lut = table.crtc_lut(randr, 256);
        assert_eq!(at(&lut, 0), (65472, 0, 0));
        assert_eq!(at(&lut, 1), (65216, 775, 128));
        assert_eq!(at(&lut, 100), (39808, 30950, 14784));
        assert_eq!(at(&lut, 128), (32640, 37691, 18944));
        assert_eq!(at(&lut, 255), (0, 65472, 37824));
    }

    /// Yserver's RANDR ramp follows the hardware size: a 256-entry one is
    /// looked up at the palette value scaled from 1023 to 255, and an empty
    /// one is Xorg's absent ramp, the value shifted to 16 bits.
    #[test]
    fn vidmode_gamma_table_scales_into_a_hardware_sized_randr_ramp() {
        let randr = identity_ramp(256);
        let lut = VidModeGammaTable::identity().crtc_lut((&randr, &randr, &randr), 256);
        assert_eq!(at(&lut, 0), (0, 0, 0));
        // Palette 1 is table[4] = 4, which scales to 4 * 255 / 1023 = 0.
        assert_eq!(lut.0[1], randr[0]);
        assert_eq!(lut.0[128], randr[513 * 255 / 1023]);
        assert_eq!(lut.0[255], 65535);

        let lut = VidModeGammaTable::identity().crtc_lut((&[], &[], &[]), 256);
        assert_eq!((lut.0[1], lut.0[255]), (4 << 6, 1023 << 6));
        assert!(
            VidModeGammaTable::identity()
                .crtc_lut((&randr, &randr, &randr), 0)
                .0
                .is_empty()
        );
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
