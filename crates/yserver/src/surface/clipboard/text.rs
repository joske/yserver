//! Text as a host clipboard holds it, where that differs from X: Windows
//! ends lines with CRLF and keeps `CF_UNICODETEXT` as NUL-terminated
//! UTF-16; X text ends lines with LF.

#![cfg_attr(not(windows), allow(dead_code))]

/// X text for a CRLF host: each LF not already after a CR becomes CRLF.
pub(crate) fn lf_to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut after_cr = false;
    for c in text.chars() {
        if c == '\n' && !after_cr {
            out.push('\r');
        }
        out.push(c);
        after_cr = c == '\r';
    }
    out
}

/// CRLF host text for X: each CRLF becomes LF; a lone CR stays.
pub(crate) fn crlf_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// X text as `CF_UNICODETEXT`: CRLF lines, UTF-16, NUL-terminated.
pub(crate) fn to_windows_units(text: &str) -> Vec<u16> {
    lf_to_crlf(text)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// `CF_UNICODETEXT` as X text: up to the first NUL, LF lines. Unpaired
/// surrogates become U+FFFD.
pub(crate) fn from_windows_units(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    crlf_to_lf(&String::from_utf16_lossy(&units[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lf_becomes_crlf() {
        assert_eq!(lf_to_crlf(""), "");
        assert_eq!(lf_to_crlf("a\nb"), "a\r\nb");
        assert_eq!(lf_to_crlf("\n\n"), "\r\n\r\n");
        assert_eq!(lf_to_crlf("é\n€\n"), "é\r\n€\r\n");
    }

    #[test]
    fn existing_crlf_and_lone_cr_are_kept() {
        assert_eq!(lf_to_crlf("a\r\nb"), "a\r\nb");
        assert_eq!(lf_to_crlf("a\rb"), "a\rb");
        assert_eq!(lf_to_crlf("a\r\r\nb"), "a\r\r\nb");
    }

    #[test]
    fn crlf_becomes_lf() {
        assert_eq!(crlf_to_lf(""), "");
        assert_eq!(crlf_to_lf("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(crlf_to_lf("a\nb"), "a\nb");
        assert_eq!(crlf_to_lf("a\rb"), "a\rb");
        // The first CR is lone, the second ends a CRLF.
        assert_eq!(crlf_to_lf("a\r\r\nb"), "a\r\nb");
    }

    #[test]
    fn windows_units_are_crlf_utf16_with_a_nul() {
        // é U+00E9, € U+20AC, 😀 U+1F600 = D83D DE00.
        assert_eq!(
            to_windows_units("é\n€😀"),
            [0x00E9, 0x000D, 0x000A, 0x20AC, 0xD83D, 0xDE00, 0x0000]
        );
        assert_eq!(to_windows_units(""), [0x0000]);
    }

    #[test]
    fn windows_units_read_up_to_the_nul_with_lf_lines() {
        assert_eq!(
            from_windows_units(&[0x00E9, 0x000D, 0x000A, 0x20AC, 0xD83D, 0xDE00, 0, 0x0041]),
            "é\n€😀"
        );
        // No terminator: the whole buffer.
        assert_eq!(from_windows_units(&[0x0061, 0x000A]), "a\n");
        // An unpaired high surrogate.
        assert_eq!(from_windows_units(&[0xD83D, 0x0061, 0]), "\u{FFFD}a");
    }
}
