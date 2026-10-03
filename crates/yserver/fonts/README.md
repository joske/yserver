# Embedded X11 core fonts

These PCF files are compiled into the server with `include_bytes!` and serve
the `built-ins` font-path element, mirroring Xorg's built-in font FPE (libXfont2
embeds `fixed` and `cursor` the same way).

They must NOT be replaced by a fontconfig substitution. Both are glyph sets whose
character codes carry meaning that no text font can stand in for:

- `cursor.pcf` — the X11 cursor font. `XCreateFontCursor(shape)` becomes
  `CreateGlyphCursor(source = shape, mask = shape + 1)`, so each char code IS a
  cursor shape. Substituting a text font renders letters instead: with the
  fontconfig `monospace` fallback, `XC_left_ptr` (68) produced the glyph pair
  `D`/`E` and the pointer rendered as the letter `E` (discussion #79).
- `nil2.pcf` — a 2x2 all-blank font, used by clients to build an *invisible*
  cursor (xterm hides the pointer with it while you type). Substituting a visible
  text font is the exact opposite of the intent.

`6x13-ISO8859-1.pcf` is libXfont2's own built-in `fixed`: Xorg answers `fixed`,
`6x13` and `-misc-fixed-medium-r-semicondensed--13-120-75-75-c-60-iso8859-1`
from it on a host with no core font packages, and clients (CDE's dthello) rely
on `fixed` always opening, with its metrics.

## Provenance

Extracted from Arch Linux `xorg-fonts-misc` (upstream `font-cursor-misc` and
`font-misc-misc`), decompressed from the shipped `.pcf.gz`:

    zcat /usr/share/fonts/misc/cursor.pcf.gz > cursor.pcf
    zcat /usr/share/fonts/misc/nil2.pcf.gz   > nil2.pcf
    zcat /usr/share/fonts/misc/6x13-ISO8859-1.pcf.gz > 6x13-ISO8859-1.pcf

## Licence

`font-cursor-misc` (`cursor.pcf`), per its upstream notice:

    "These "glyphs" are unencumbered"

`font-misc-misc` (`nil2.pcf`, `6x13-ISO8859-1.pcf`) is distributed under the MIT/X11 licence used by
the X.Org font packages.
