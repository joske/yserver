//! XKB SetGeometry (#171 phase 4e), name only: Xorg's `ProcXkbSetGeometry`
//! / `_XkbSetGeometry` (xkb/xkb.c) decide whether a geometry is accepted
//! by building it (`_CheckSetGeom` and the `XkbAddGeom*` allocators of
//! xkb/XKBGAlloc.c). [`check_set_geometry`] ports that walk literally —
//! every bounds, count, index and atom check, in Xorg's order, with the
//! allocators' name lookups that decide the counts Xorg compares — but
//! keeps only what those checks read: the shape, color and section counts
//! and each section's rows of key names. The geometry itself isn't stored
//! (review outcome: geometry is name-only for #171); on success the caller
//! stores its name, as `_XkbSetGeometry` does in `names->geometry`.
//!
//! Offsets are into the whole request (the 4-byte header included), so they
//! read as Xorg's pointer arithmetic from `stuff`.

use super::reply::{
    BAD_ALLOC, BAD_ATOM, BAD_LENGTH, BAD_MATCH, BAD_VALUE, XkbError, err_code2, err_code3,
    err_code4,
};

/// `sz_xkbSetGeometryReq`.
const SET_GEOMETRY_REQ_SIZE: usize = 28;
/// Wire sizes (`sz_xkb*WireDesc`).
const SHAPE_WIRE_SIZE: usize = 8;
const OUTLINE_WIRE_SIZE: usize = 4;
const POINT_WIRE_SIZE: usize = 4;
const SECTION_WIRE_SIZE: usize = 20;
const ROW_WIRE_SIZE: usize = 8;
const KEY_WIRE_SIZE: usize = 8;
const OVERLAY_WIRE_SIZE: usize = 8;
const OVERLAY_ROW_WIRE_SIZE: usize = 4;
const OVERLAY_KEY_WIRE_SIZE: usize = 8;
const DOODAD_WIRE_SIZE: usize = 20;
/// `XkbKeyNameLength`.
const KEY_NAME_LENGTH: usize = 4;
/// Doodad types (`Xkb*Doodad`).
const OUTLINE_DOODAD: u8 = 1;
const SOLID_DOODAD: u8 = 2;
const TEXT_DOODAD: u8 = 3;
const INDICATOR_DOODAD: u8 = 4;
const LOGO_DOODAD: u8 = 5;

fn u16_at(req: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([req[at], req[at + 1]])
}

fn u32_at(req: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([req[at], req[at + 1], req[at + 2], req[at + 3]])
}

/// `xkbSetGeometryReq`'s fields after the 4-byte header that the checks
/// read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SetGeometryHeader {
    pub device_spec: u16,
    pub n_shapes: u8,
    pub n_sections: u8,
    pub name: u32,
    pub width_mm: u16,
    pub height_mm: u16,
    pub n_properties: u16,
    pub n_colors: u16,
    pub n_doodads: u16,
    pub n_key_aliases: u16,
    pub base_color_ndx: u8,
    pub label_color_ndx: u8,
}

impl SetGeometryHeader {
    /// The header of a whole request `req` (at least 28 bytes).
    pub(crate) fn parse(req: &[u8]) -> Self {
        Self {
            device_spec: u16_at(req, 4),
            n_shapes: req[6],
            n_sections: req[7],
            name: u32_at(req, 8),
            width_mm: u16_at(req, 12),
            height_mm: u16_at(req, 14),
            n_properties: u16_at(req, 16),
            n_colors: u16_at(req, 18),
            n_doodads: u16_at(req, 20),
            n_key_aliases: u16_at(req, 22),
            base_color_ndx: req[24],
            label_color_ndx: req[25],
        }
    }
}

/// A C string from wire bytes: up to the first NUL (Xorg compares the
/// counted strings it copied with `strcmp`).
fn c_string(bytes: &[u8]) -> &[u8] {
    bytes
        .iter()
        .position(|&b| b == 0)
        .map_or(bytes, |n| &bytes[..n])
}

/// `strncmp(a, b, XkbKeyNameLength) == 0` over two wire key names.
fn key_names_equal(a: &[u8], b: &[u8]) -> bool {
    for i in 0..KEY_NAME_LENGTH {
        if a[i] != b[i] {
            return false;
        }
        if a[i] == 0 {
            return true;
        }
    }
    true
}

/// What the checks read back of the geometry under construction: the
/// allocators' lookups (shapes by name atom, colors by spec, sections by
/// name atom) decide the counts `_CheckSetGeom` compares and the rows an
/// overlay key is looked up in. (Doodads, overlays and overlay rows are
/// looked up by name too, but nothing checks their counts.)
#[derive(Default)]
struct Geom<'a> {
    shapes: Vec<u32>,
    colors: Vec<&'a [u8]>,
    sections: Vec<Section<'a>>,
}

#[derive(Default)]
struct Section<'a> {
    name: u32,
    /// Each row's key names (wire bytes).
    rows: Vec<Vec<&'a [u8]>>,
}

/// The walk: Xorg's `wire`, its bounds and length checks.
struct Walk<'a> {
    req: &'a [u8],
    atom_valid: &'a dyn Fn(u32) -> bool,
}

impl<'a> Walk<'a> {
    /// `_XkbCheckRequestBounds`: BadLength (Xorg's stale errorValue, 0).
    fn bounds(&self, from: usize, to: usize) -> Result<(), XkbError> {
        if from < to && from < self.req.len() && to <= self.req.len() {
            Ok(())
        } else {
            Err(XkbError {
                code: BAD_LENGTH,
                value: 0,
            })
        }
    }

    /// `CHK_ATOM_ONLY`: `None` or an atom that isn't valid is BadAtom.
    fn atom_only(&self, atom: u32) -> Result<(), XkbError> {
        if atom == 0 || !(self.atom_valid)(atom) {
            return Err(XkbError {
                code: BAD_ATOM,
                value: atom,
            });
        }
        Ok(())
    }

    /// `_GetCountedString`: a CARD16 length and the bytes, padded; both
    /// ends must be inside the request (BadValue, stale errorValue 0).
    fn counted_string(&self, at: &mut usize) -> Result<&'a [u8], XkbError> {
        let bad = XkbError {
            code: BAD_VALUE,
            value: 0,
        };
        let words = self.req.len() / 4;
        if words < (*at + 2).div_ceil(4) {
            return Err(bad);
        }
        let len = usize::from(u16_at(self.req, *at));
        let next = *at + super::padded(len + 2);
        if words < next.div_ceil(4) {
            return Err(bad);
        }
        let s = c_string(&self.req[*at + 2..*at + 2 + len]);
        *at = next;
        Ok(s)
    }

    /// `_CheckSetDoodad`, for a section's doodad or the geometry's own
    /// (the same checks).
    fn doodad(&self, at: &mut usize, geom: &Geom<'a>) -> Result<(), XkbError> {
        let d = *at;
        self.bounds(d, d + DOODAD_WIRE_SIZE)?;
        self.atom_only(u32_at(self.req, d))?;
        let (n_colors, n_shapes) = (geom.colors.len(), geom.shapes.len());
        let color_err = |code: u32, ndx: u8| XkbError {
            code: BAD_MATCH,
            value: err_code3(code, count(n_colors), u32::from(ndx)),
        };
        let shape_err = |code: u32, ndx: u8| XkbError {
            code: BAD_MATCH,
            value: err_code3(code, count(n_shapes), u32::from(ndx)),
        };
        let mut wire = d + DOODAD_WIRE_SIZE;
        let r = self.req;
        match r[d + 4] {
            OUTLINE_DOODAD | SOLID_DOODAD => {
                if usize::from(r[d + 12]) >= n_colors {
                    return Err(color_err(0x40, r[d + 12]));
                }
                if usize::from(r[d + 13]) >= n_shapes {
                    return Err(shape_err(0x41, r[d + 13]));
                }
            }
            TEXT_DOODAD => {
                if usize::from(r[d + 16]) >= n_colors {
                    return Err(color_err(0x42, r[d + 16]));
                }
                self.counted_string(&mut wire)?;
                self.counted_string(&mut wire)?;
            }
            INDICATOR_DOODAD => {
                if usize::from(r[d + 13]) >= n_colors {
                    return Err(color_err(0x43, r[d + 13]));
                }
                if usize::from(r[d + 14]) >= n_colors {
                    return Err(color_err(0x44, r[d + 14]));
                }
                if usize::from(r[d + 12]) >= n_shapes {
                    return Err(shape_err(0x45, r[d + 12]));
                }
            }
            LOGO_DOODAD => {
                if usize::from(r[d + 12]) >= n_colors {
                    return Err(color_err(0x46, r[d + 12]));
                }
                if usize::from(r[d + 13]) >= n_shapes {
                    return Err(shape_err(0x47, r[d + 13]));
                }
                self.counted_string(&mut wire)?;
            }
            other => {
                return Err(XkbError {
                    code: BAD_VALUE,
                    value: err_code2(0x4f, u32::from(other)),
                });
            }
        }
        *at = wire;
        Ok(())
    }

    /// `_CheckSetOverlay` for an overlay of `section`.
    fn overlay(&self, at: &mut usize, section: &Section<'a>) -> Result<(), XkbError> {
        let o = *at;
        self.bounds(o, o + OVERLAY_WIRE_SIZE)?;
        let name = u32_at(self.req, o);
        self.atom_only(name)?;
        let n_rows = self.req[o + 4];
        let mut r = o + OVERLAY_WIRE_SIZE;
        for row in 0..usize::from(n_rows) {
            self.bounds(r, r + OVERLAY_ROW_WIRE_SIZE)?;
            let (row_under, n_keys) = (self.req[r], self.req[r + 1]);
            let num_rows = section.rows.len();
            // Xvfb 21.1.24 refuses a row over the row past the last too
            // (captured: rowUnder = num_rows draws this error; the 21.1.22
            // source still tests `rowUnder > num_rows`, whose NULL overlay
            // row would then fail on its first key instead).
            if usize::from(row_under) >= num_rows {
                return Err(XkbError {
                    code: BAD_MATCH,
                    value: err_code4(0x20, count(row), count(num_rows), u32::from(row_under)),
                });
            }
            let under_row = section.rows.get(usize::from(row_under));
            let mut k = r + OVERLAY_ROW_WIRE_SIZE;
            for key in 0..usize::from(n_keys) {
                self.bounds(k, k + OVERLAY_KEY_WIRE_SIZE)?;
                let under = &self.req[k + KEY_NAME_LENGTH..k + 2 * KEY_NAME_LENGTH];
                let found = under_row
                    .is_some_and(|keys| keys.iter().any(|name| key_names_equal(under, name)));
                if !found {
                    return Err(XkbError {
                        code: BAD_MATCH,
                        value: err_code3(0x21, count(row), count(key)),
                    });
                }
                k += OVERLAY_KEY_WIRE_SIZE;
            }
            r = k;
        }
        *at = r;
        Ok(())
    }
}

/// A count as C's `int` in an `_XkbErrCode*` argument.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Port of `ProcXkbSetGeometry`'s checks after its size and BadAccess
/// checks (the core loop's): `CHK_ATOM_OR_NONE(name)`, then
/// `_CheckSetGeom` over the whole geometry. `atom_valid` = Xorg's
/// `ValidAtom`. Nothing is kept but whether it passed.
pub(crate) fn check_set_geometry(
    h: &SetGeometryHeader,
    req: &[u8],
    atom_valid: &dyn Fn(u32) -> bool,
) -> Result<(), XkbError> {
    if h.name != 0 && !atom_valid(h.name) {
        return Err(XkbError {
            code: BAD_ATOM,
            value: h.name,
        });
    }
    let w = Walk { req, atom_valid };
    let mut geom = Geom::default();
    let mut at = SET_GEOMETRY_REQ_SIZE;
    // label_font, then the properties (XkbAddGeomProperty never fails).
    w.counted_string(&mut at)?;
    for _ in 0..h.n_properties {
        w.counted_string(&mut at)?;
        w.counted_string(&mut at)?;
    }

    let n_colors = u32::from(h.n_colors);
    if h.n_colors < 2 {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code3(0x01, 2, n_colors),
        });
    }
    for ndx in [h.base_color_ndx, h.label_color_ndx] {
        if u16::from(ndx) > h.n_colors {
            return Err(XkbError {
                code: BAD_MATCH,
                value: err_code3(0x03, n_colors, u32::from(ndx)),
            });
        }
    }
    if h.label_color_ndx == h.base_color_ndx {
        return Err(XkbError {
            code: BAD_MATCH,
            value: err_code3(
                0x04,
                u32::from(h.base_color_ndx),
                u32::from(h.label_color_ndx),
            ),
        });
    }
    for _ in 0..h.n_colors {
        // XkbAddGeomColor: a spec already there is the same color.
        let spec = w.counted_string(&mut at)?;
        if !geom.colors.contains(&spec) {
            geom.colors.push(spec);
        }
    }
    if usize::from(h.n_colors) != geom.colors.len() {
        return Err(XkbError {
            code: BAD_MATCH,
            value: err_code3(0x05, n_colors, count(geom.colors.len())),
        });
    }

    // _CheckSetShapes
    if h.n_shapes < 1 {
        return Err(XkbError {
            code: BAD_VALUE,
            value: err_code2(0x06, u32::from(h.n_shapes)),
        });
    }
    for _ in 0..h.n_shapes {
        w.bounds(at, at + SHAPE_WIRE_SIZE)?;
        let name = u32_at(req, at);
        // XkbAddGeomShape: NULL (BadAlloc) for None, else the shape of
        // that name or a new one.
        if name == 0 {
            return Err(XkbError {
                code: BAD_ALLOC,
                value: 0,
            });
        }
        if !geom.shapes.contains(&name) {
            geom.shapes.push(name);
        }
        let n_outlines = req[at + 4];
        at += SHAPE_WIRE_SIZE;
        for _ in 0..n_outlines {
            w.bounds(at, at + OUTLINE_WIRE_SIZE)?;
            let n_points = usize::from(req[at]);
            at += OUTLINE_WIRE_SIZE;
            for _ in 0..n_points {
                w.bounds(at, at + POINT_WIRE_SIZE)?;
                at += POINT_WIRE_SIZE;
            }
        }
    }
    if geom.shapes.len() != usize::from(h.n_shapes) {
        return Err(XkbError {
            code: BAD_MATCH,
            value: err_code3(0x07, count(geom.shapes.len()), u32::from(h.n_shapes)),
        });
    }

    // _CheckSetSections
    for _ in 0..h.n_sections {
        w.bounds(at, at + SECTION_WIRE_SIZE)?;
        let name = u32_at(req, at);
        w.atom_only(name)?;
        let (n_rows, n_doodads, n_overlays) = (req[at + 15], req[at + 16], req[at + 17]);
        // XkbAddGeomSection: the section of that name (its rows grow) or a
        // new one.
        let s = match geom.sections.iter().position(|s| s.name == name) {
            Some(s) => s,
            None => {
                geom.sections.push(Section {
                    name,
                    ..Section::default()
                });
                geom.sections.len() - 1
            }
        };
        at += SECTION_WIRE_SIZE;
        for _ in 0..n_rows {
            w.bounds(at, at + ROW_WIRE_SIZE)?;
            let n_keys = req[at + 4];
            at += ROW_WIRE_SIZE;
            let mut keys = Vec::with_capacity(usize::from(n_keys));
            for _ in 0..n_keys {
                w.bounds(at, at + KEY_WIRE_SIZE)?;
                keys.push(&req[at..at + KEY_NAME_LENGTH]);
                let (shape, color) = (req[at + 6], req[at + 7]);
                if usize::from(shape) >= geom.shapes.len() {
                    return Err(XkbError {
                        code: BAD_MATCH,
                        value: err_code3(0x10, u32::from(shape), count(geom.shapes.len())),
                    });
                }
                if usize::from(color) >= geom.colors.len() {
                    return Err(XkbError {
                        code: BAD_MATCH,
                        value: err_code3(0x11, u32::from(color), count(geom.colors.len())),
                    });
                }
                at += KEY_WIRE_SIZE;
            }
            geom.sections[s].rows.push(keys);
        }
        for _ in 0..n_doodads {
            w.doodad(&mut at, &geom)?;
        }
        for _ in 0..n_overlays {
            w.overlay(&mut at, &geom.sections[s])?;
        }
    }

    for _ in 0..h.n_doodads {
        w.doodad(&mut at, &geom)?;
    }
    for _ in 0..h.n_key_aliases {
        w.bounds(at, at + 2 * KEY_NAME_LENGTH)?;
        // XkbAddGeomKeyAlias: NULL (BadAlloc) for an empty alias or real
        // name.
        if req[at + KEY_NAME_LENGTH] == 0 || req[at] == 0 {
            return Err(XkbError {
                code: BAD_ALLOC,
                value: 0,
            });
        }
        at += 2 * KEY_NAME_LENGTH;
    }
    Ok(())
}
