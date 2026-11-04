//! A monitor's physical size from its EDID (VESA E-EDID 1.3/1.4 base
//! block), for a window system that hands out the raw block (Windows).

/// The fixed EDID header.
const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
/// The first detailed timing descriptor (the preferred mode).
const FIRST_DTD: usize = 54;

/// The image size in millimetres: the first detailed timing descriptor's,
/// else the base block's centimetres (bytes 21 and 22); `None` when the
/// block is not an EDID or gives no size (a projector, an aspect ratio).
pub(crate) fn size_mm(edid: &[u8]) -> Option<(u32, u32)> {
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let dtd = &edid[FIRST_DTD..FIRST_DTD + 18];
    // A non-zero pixel clock: a timing, not a display descriptor.
    if dtd[0] != 0 || dtd[1] != 0 {
        let width = u32::from(dtd[12]) | (u32::from(dtd[14] >> 4) << 8);
        let height = u32::from(dtd[13]) | (u32::from(dtd[14] & 0x0F) << 8);
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }
    let (cm_w, cm_h) = (u32::from(edid[21]), u32::from(edid[22]));
    // Both zero: unknown; one zero: the other byte is an aspect ratio.
    (cm_w > 0 && cm_h > 0).then_some((cm_w * 10, cm_h * 10))
}

#[cfg(test)]
mod tests {
    use super::size_mm;

    /// A 1920x1080@60 EDID: 53x30 cm in the base block, 527x296 mm in the
    /// preferred timing (`0x0F` 0x28 low bytes, `0x21` high nibbles: width
    /// 0x20F = 527, height 0x128 = 296). `edid-decode` on these bytes says
    /// "Maximum image size: 53 cm x 30 cm" and "DTD 1: 1920x1080 60.000 Hz
    /// 16:9 ... (527 mm x 296 mm)".
    fn edid() -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        e[8..10].copy_from_slice(&[0x10, 0xAC]); // Manufacturer "DEL".
        e[18] = 1; // EDID 1.4.
        e[19] = 4;
        e[20] = 0xA5; // Digital, 8 bpc, DisplayPort.
        e[21] = 53;
        e[22] = 30;
        e[23] = 120; // Gamma 2.2.
        e[24] = 0x06;
        // Standard timings: unused.
        for b in e[38..54].iter_mut() {
            *b = 0x01;
        }
        // 148.5 MHz; 1920 + 280 blank; 1080 + 45 blank; 88/44, 4/5 sync.
        e[54..72].copy_from_slice(&[
            0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, 0x45, 0x00, 0x0F, 0x28,
            0x21, 0x00, 0x00, 0x1E,
        ]);
        // Descriptors 2-4: dummy (tag 0x10).
        for d in [72, 90, 108] {
            e[d + 3] = 0x10;
        }
        let sum = e[..127].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        e[127] = 0u8.wrapping_sub(sum);
        e
    }

    #[test]
    fn the_preferred_timing_gives_millimetres() {
        assert_eq!(size_mm(&edid()), Some((527, 296)));
    }

    #[test]
    fn without_a_timing_size_the_centimetres_count() {
        let mut e = edid();
        e[66] = 0;
        e[67] = 0;
        e[68] = 0;
        assert_eq!(size_mm(&e), Some((530, 300)));
        // No timing at all: a display descriptor first.
        let mut e = edid();
        e[54..72].copy_from_slice(&[
            0, 0, 0, 0xFC, 0, b'X', b'\n', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(size_mm(&e), Some((530, 300)));
    }

    #[test]
    fn no_size_or_no_edid_is_unknown() {
        let mut e = edid();
        e[66..69].fill(0);
        e[21] = 0;
        e[22] = 0;
        assert_eq!(size_mm(&e), None);
        e[21] = 0x4F; // An aspect ratio (landscape), no size.
        assert_eq!(size_mm(&e), None);
        assert_eq!(size_mm(&edid()[..127]), None, "short");
        let mut bad = edid();
        bad[0] = 1;
        assert_eq!(size_mm(&bad), None, "no header");
    }
}
