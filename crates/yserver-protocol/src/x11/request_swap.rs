//! Per-opcode request-body swap tables for inbound BE-client requests.
//!
//! After `read_request` returns the body, the per-client reader thread
//! calls `swap_request_body(opcode, byte_order, &mut body)` which looks
//! up the opcode in this module's table and byte-swaps each typed
//! field in place. After the swap the body is in LE form and the
//! existing dispatch / decoder code (which uses `read_u16_le` /
//! `read_u32_le` everywhere) runs unchanged.
//!
//! Tables are conservative: when an opcode is missing or unknown the
//! body passes through unchanged. xts coverage will guide which
//! opcodes need entries first.
//!
//! Field offsets are relative to the start of the body — i.e., the
//! 4-byte request header is **not** included.

use super::{
    ClientByteOrder,
    wire_swap::{FieldEntry, FieldKind, swap_in_place},
};

/// Apply the per-opcode swap table for `major` (and `minor`, for
/// extensions) to `body`. No-op for little-endian clients, or for
/// (opcode, minor) pairs without a registered table.
///
/// For core opcodes (1–127), `minor` is the request's `data` byte and
/// is ignored by the swap tables here (core layouts are uniquely
/// determined by the major). For extensions (128+), `minor` is the
/// extension's request type and selects the correct field-layout.
pub fn swap_request_body(major: u8, minor: u8, byte_order: ClientByteOrder, body: &mut [u8]) {
    if matches!(byte_order, ClientByteOrder::LittleEndian) {
        return;
    }
    let entries = if major < 128 {
        core_request_swap_table(major)
    } else {
        extension_request_swap_table(major, minor)
    };
    if let Some(entries) = entries {
        swap_in_place(entries, byte_order, body);
    }
    if major == 137 && minor == 23 {
        swap_xi_change_feedback_control(byte_order, body);
    } else if major == 137 && minor == 35 {
        swap_xi_change_device_control(byte_order, body);
    }
}

/// ChangeFeedbackControl's trailing control is a tagged union selected by
/// request byte 5 (`feedbackid`, which is actually the feedback class).
fn swap_xi_change_feedback_control(byte_order: ClientByteOrder, body: &mut [u8]) {
    use FieldEntry::Fixed;
    use FieldKind::{I16, U16, U32};

    let entries: &[FieldEntry] = match body.get(5) {
        // xKbdFeedbackCtl at body[8..]
        Some(0) => &[
            Fixed {
                offset: 10,
                kind: U16,
            },
            Fixed {
                offset: 16,
                kind: I16,
            },
            Fixed {
                offset: 18,
                kind: I16,
            },
            Fixed {
                offset: 20,
                kind: U32,
            },
            Fixed {
                offset: 24,
                kind: U32,
            },
        ],
        // xPtrFeedbackCtl at body[8..]
        Some(1) => &[
            Fixed {
                offset: 10,
                kind: U16,
            },
            Fixed {
                offset: 14,
                kind: I16,
            },
            Fixed {
                offset: 16,
                kind: I16,
            },
            Fixed {
                offset: 18,
                kind: I16,
            },
        ],
        _ => &[],
    };
    swap_in_place(entries, byte_order, body);
}

/// `RRChangeOutputProperty` / `RRChangeProviderProperty`'s trailing value
/// payload (from body offset 20) is a tagged union selected by the format
/// byte at offset 12: 8 = bytes (untouched), 16 = `INT16[]`, 32 =
/// `INT32[]`. Mirrors Xorg's `SProcRRChangeOutputProperty` `SwapRestS` /
/// `SwapRestL` dispatch.
fn swap_change_output_property_tail(byte_order: ClientByteOrder, body: &mut [u8]) {
    use FieldEntry::ElementArrayTail;

    let entries: &[FieldEntry] = match body.get(12) {
        Some(16) => &[ElementArrayTail {
            from: 20,
            kind: FieldKind::U16,
        }],
        Some(32) => &[ElementArrayTail {
            from: 20,
            kind: FieldKind::U32,
        }],
        _ => &[],
    };
    swap_in_place(entries, byte_order, body);
}

fn swap_xi_change_device_control(byte_order: ClientByteOrder, body: &mut [u8]) {
    use FieldEntry::{ElementArrayTail, Fixed};
    use FieldKind::{U16, U32};

    swap_in_place(
        &[
            Fixed {
                offset: 4,
                kind: U16,
            },
            Fixed {
                offset: 6,
                kind: U16,
            },
            ElementArrayTail {
                from: 12,
                kind: U32,
            },
        ],
        byte_order,
        body,
    );
}

/// Dispatch to a per-extension swap table by major opcode. yserver
/// assigns a fixed major to every supported extension (see the
/// `XI2_MAJOR_OPCODE` / `XFIXES_MAJOR_OPCODE` constants in
/// `core_loop::process_request`), so the major→table mapping is
/// stable. Other extensions pass through (their handlers either
/// already read fields with explicit byte_order or are LE-only paths
/// that no xts5 BE-byte-sex tests exercise yet).
const fn extension_request_swap_table(major: u8, minor: u8) -> Option<&'static [FieldEntry]> {
    match major {
        // RANDR extension (major fixed at 128 in
        // `process_request::RANDR_MAJOR_OPCODE`).
        128 => randr_request_swap_table(minor),
        // XInput extension (major fixed at 137 in
        // `process_request::XI2_MAJOR_OPCODE`).
        137 => xi_request_swap_table(minor),
        // XFree86-VidModeExtension (major fixed at 153). ValidateModeLine is
        // intentionally decoded in the handler because its layout depends on
        // per-client protocol-version state unavailable here.
        153 => xf86vidmode_request_swap_table(minor),
        _ => None,
    }
}

const fn xf86vidmode_request_swap_table(minor: u8) -> Option<&'static [FieldEntry]> {
    use FieldEntry::Fixed;
    use FieldKind::U16;

    // `screen(u16) + pad(u16)`, shared by the ordinary screen-scoped reads.
    const SCREEN: &[FieldEntry] = &[Fixed {
        offset: 0,
        kind: U16,
    }];

    Some(match minor {
        // QueryVersion (0) has an empty body. ValidateModeLine (9) stays
        // unswapped for its version-aware parser. Rejected write requests are
        // not inspected.
        1 | 4 | 6 | 11 | 13 | 16 | 19 | 20 => SCREEN,
        // GetGammaRamp: screen(u16), size(u16).
        17 => &[
            Fixed {
                offset: 0,
                kind: U16,
            },
            Fixed {
                offset: 2,
                kind: U16,
            },
        ],
        14 => &[
            Fixed {
                offset: 0,
                kind: U16,
            },
            Fixed {
                offset: 2,
                kind: U16,
            },
        ],
        _ => return None,
    })
}

const fn randr_request_swap_table(minor: u8) -> Option<&'static [FieldEntry]> {
    use FieldEntry::{Custom, ElementArrayTail, Fixed};
    use FieldKind::{I16, U16, U32};

    macro_rules! u32f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U32,
            }
        };
    }
    macro_rules! u16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U16,
            }
        };
    }

    macro_rules! i16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: I16,
            }
        };
    }

    // Offsets are body-relative (the 4-byte request header is already
    // stripped) and taken field-by-field from randrproto.h, NOT from the
    // request's prose description. Every minor whose handler reads a field
    // needs an entry: a big-endian client's xid read back little-endian is a
    // silent mis-parse that surfaces as a bogus BadWindow/BadOutput/BadCrtc.
    // Big-endian clients are not hypothetical here — X11 is network
    // transparent, so `ssh -X` from a big-endian host (AIX/POWER, s390x,
    // Solaris/SPARC) drives a big-endian client against this server over the
    // LOCAL socket.
    Some(match minor {
        // GetScreenInfo / GetScreenSizeRange / GetScreenResources /
        // GetScreenResourcesCurrent / GetOutputPrimary / GetProviders:
        // a lone Window. GetCrtcGammaSize / GetCrtcGamma / GetCrtcTransform /
        // GetPanning / ListOutputProperties: a lone RRCrtc/RROutput.
        5 | 6 | 8 | 10 | 22 | 23 | 25 | 27 | 28 | 31 | 32 | 36 => &[u32f!(0)],
        // SetScreenConfig: drawable, timestamp, configTimestamp, sizeID,
        // rotation, rate.
        2 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u16f!(12),
            u16f!(14),
            u16f!(16),
        ],
        // SelectInput: window, enable.
        4 => &[u32f!(0), u16f!(4)],
        // SetScreenSize: window, width, height, mmWidth, mmHeight.
        7 => &[u32f!(0), u16f!(4), u16f!(6), u32f!(8), u32f!(12)],
        // GetOutputInfo / GetCrtcInfo: resource + configTimestamp.
        // QueryOutputProperty / DeleteOutputProperty / SetOutputPrimary /
        // AddOutputMode / DeleteOutputMode / DeleteMonitor / provider pairs:
        // two 32-bit ids.
        9 | 11 | 14 | 18 | 19 | 20 | 30 | 37 | 40 | 44 => &[u32f!(0), u32f!(4)],
        // GetOutputProperty / GetProviderProperty: output/provider, property,
        // type, longOffset, longLength (the trailing BOOLs are bytes).
        15 | 41 => &[u32f!(0), u32f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // ConfigureOutputProperty / ConfigureProviderProperty: ids, then BOOLs
        // (untouched), then a trailing INT32[] of valid values (Xorg
        // `SProcRRConfigureOutputProperty` swaps it with `SwapRestL`).
        12 | 38 => &[
            u32f!(0),
            u32f!(4),
            ElementArrayTail {
                from: 12,
                kind: U32,
            },
        ],
        // ChangeOutputProperty / ChangeProviderProperty: output, property,
        // type, then format/mode/pad bytes, then nUnits, then a value payload
        // whose unit width depends on the format byte at offset 12 — Xorg's
        // `SProcRRChangeOutputProperty` picks `SwapRestS`/`SwapRestL`
        // accordingly (format 8 is untouched).
        13 | 39 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u32f!(16),
            Custom(swap_change_output_property_tail),
        ],
        // DestroyMode: a lone RRMode. FreeLease: RRLease + terminate byte.
        17 | 46 => &[u32f!(0)],
        // SetCrtcConfig: crtc, timestamp, configTimestamp, x, y, mode,
        // rotation, then a trailing RROutput array.
        21 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            i16f!(12),
            i16f!(14),
            u32f!(16),
            u16f!(20),
            ElementArrayTail {
                from: 24,
                kind: U32,
            },
        ],
        super::randr::RR_SET_CRTC_GAMMA => {
            &[u32f!(0), u16f!(4), ElementArrayTail { from: 8, kind: U16 }]
        }
        // SetProviderOutputSource / SetProviderOffloadSink: provider, peer,
        // configTimestamp. GetProviderInfo: provider + configTimestamp.
        33..=35 => &[u32f!(0), u32f!(4), u32f!(8)],
        // GetMonitors: window + get_active byte.
        42 => &[u32f!(0)],
        _ => return None,
    })
}

#[allow(clippy::too_many_lines)]
const fn core_request_swap_table(opcode: u8) -> Option<&'static [FieldEntry]> {
    use FieldEntry::{ElementArrayTail, Fixed, OpaqueTail};
    use FieldKind::{I16, U16, U32};

    macro_rules! u32f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U32,
            }
        };
    }
    macro_rules! u16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U16,
            }
        };
    }
    macro_rules! i16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: I16,
            }
        };
    }

    Some(match opcode {
        // 1 CreateWindow
        // body: window(u32) parent(u32) x(i16) y(i16) w(u16) h(u16)
        //       border_w(u16) class(u16) visual(u32) value-mask(u32) values(u32[])
        1 => &[
            u32f!(0),
            u32f!(4),
            i16f!(8),
            i16f!(10),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u32f!(20),
            u32f!(24),
            ElementArrayTail {
                from: 28,
                kind: U32,
            },
        ],
        // 2 ChangeWindowAttributes: window(u32) mask(u32) values(u32[])
        2 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 3-15: most are window-only u32 at offset 0
        3 | 4 | 5 | 6 | 8 | 9 | 10 | 11 | 13 | 14 | 15 | 21 | 23 | 38 | 79 | 81 | 82 | 83 | 95
        | 113 => &[u32f!(0)],
        // 7 ReparentWindow: window(u32) parent(u32) x(i16) y(i16)
        7 => &[u32f!(0), u32f!(4), i16f!(8), i16f!(10)],
        // 12 ConfigureWindow: window(u32) mask(u16) pad(u16) values(u32[])
        12 => &[u32f!(0), u16f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 16 InternAtom: nbytes(u16 at offset 0), pad(u16 at 2), name(opaque from 4)
        // The byte `data` of header is `only_if_exists` — already u8, no swap.
        16 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 17 GetAtomName: atom(u32 at 0)
        17 => &[u32f!(0)],
        // 18 ChangeProperty: window(u32) property(u32) type(u32) format(u8)
        //                   pad(u8 × 3) data_len(u32) data(opaque, format-
        //                   dependent — swapping per format would belong in
        //                   a Custom handler; for now treat as opaque so we
        //                   don't corrupt e.g. format=32 data).
        // The format byte at offset 12 is u8 — no swap needed. data_len
        // at offset 16 is u32 — must be swapped.
        18 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u32f!(16),
            OpaqueTail { from: 20 },
        ],
        // 19 DeleteProperty: window(u32) property(u32)
        19 => &[u32f!(0), u32f!(4)],
        // 20 GetProperty: window(u32) property(u32) type(u32) long_off(u32) long_len(u32)
        20 => &[u32f!(0), u32f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 22 SetSelectionOwner: owner(u32) selection(u32) time(u32)
        22 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 24 ConvertSelection: requestor(u32) selection(u32) target(u32) property(u32) time(u32)
        24 => &[u32f!(0), u32f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 25 SendEvent: destination(u32) event_mask(u32) event(32 bytes opaque)
        // The event template is in the *sender's* byte order; the SendEvent
        // handler re-encodes per recipient. Swap only the destination/mask.
        25 => &[u32f!(0), u32f!(4), OpaqueTail { from: 8 }],
        // 26 GrabPointer: window(u32) event_mask(u16) [u8 u8] confine_to(u32) cursor(u32) time(u32)
        26 => &[u32f!(0), u16f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 27 UngrabPointer: time(u32)
        27 => &[u32f!(0)],
        // 28 GrabButton: window(u32) event_mask(u16) [u8 u8] confine_to(u32) cursor(u32) [u8 button]
        //               [u8 pad] modifiers(u16)
        28 => &[u32f!(0), u16f!(4), u32f!(8), u32f!(12), u16f!(20)],
        // 29 UngrabButton: window(u32) modifiers(u16)
        29 => &[u32f!(0), u16f!(4)],
        // 30 ChangeActivePointerGrab: cursor(u32) time(u32) event_mask(u16)
        30 => &[u32f!(0), u32f!(4), u16f!(8)],
        // 31 GrabKeyboard: window(u32) time(u32) [u8 u8 u8 u8]
        31 => &[u32f!(0), u32f!(4)],
        // 32 UngrabKeyboard: time(u32)
        32 => &[u32f!(0)],
        // 33 GrabKey: window(u32) modifiers(u16) [u8 u8 u8 u8]
        33 => &[u32f!(0), u16f!(4)],
        // 34 UngrabKey: window(u32) modifiers(u16)
        34 => &[u32f!(0), u16f!(4)],
        // 35 AllowEvents: time(u32). Mode is in header.data.
        35 => &[u32f!(0)],
        // 39 GetMotionEvents: window(u32) start(u32) stop(u32)
        39 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 40 TranslateCoordinates: src(u32) dst(u32) src_x(i16) src_y(i16)
        40 => &[u32f!(0), u32f!(4), i16f!(8), i16f!(10)],
        // 41 WarpPointer: src(u32) dst(u32) src_x(i16) src_y(i16) src_w(u16) src_h(u16)
        //                dst_x(i16) dst_y(i16)
        41 => &[
            u32f!(0),
            u32f!(4),
            i16f!(8),
            i16f!(10),
            u16f!(12),
            u16f!(14),
            i16f!(16),
            i16f!(18),
        ],
        // 42 SetInputFocus: focus(u32) time(u32)
        42 => &[u32f!(0), u32f!(4)],
        // 45 OpenFont: fid(u32) name_len(u16) [pad u16] name(opaque)
        45 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 46 CloseFont, 47 QueryFont: font(u32)
        46 | 47 => &[u32f!(0)],
        // 48 QueryTextExtents: font_or_gc(u32) string(u16[])
        48 => &[u32f!(0), ElementArrayTail { from: 4, kind: U16 }],
        // 49/50 ListFonts/ListFontsWithInfo: max(u16) nlen(u16) name(opaque)
        49 | 50 => &[u16f!(0), u16f!(2), OpaqueTail { from: 4 }],
        // 51 SetFontPath: nstr(u16) [u16 pad] strings(opaque)
        51 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 53 CreatePixmap: pid(u32) drawable(u32) w(u16) h(u16). depth in header.data.
        53 => &[u32f!(0), u32f!(4), u16f!(8), u16f!(10)],
        // 54 FreePixmap: pixmap(u32)
        54 => &[u32f!(0)],
        // 55 CreateGC: cid(u32) drawable(u32) mask(u32) values(u32[])
        55 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            ElementArrayTail {
                from: 12,
                kind: U32,
            },
        ],
        // 56 ChangeGC: gc(u32) mask(u32) values(u32[])
        56 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 57 CopyGC: src(u32) dst(u32) mask(u32)
        57 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 58 SetDashes: gc(u32) dash_offset(u16) ndashes(u16) dashes(opaque)
        58 => &[u32f!(0), u16f!(4), u16f!(6), OpaqueTail { from: 8 }],
        // 59 SetClipRectangles: gc(u32) clip_x(i16) clip_y(i16) rects(i16[]/u16[])
        59 => &[
            u32f!(0),
            i16f!(4),
            i16f!(6),
            ElementArrayTail { from: 8, kind: U16 },
        ],
        // 60 FreeGC: gc(u32)
        60 => &[u32f!(0)],
        // 61 ClearArea: window(u32) x(i16) y(i16) w(u16) h(u16)
        61 => &[u32f!(0), i16f!(4), i16f!(6), u16f!(8), u16f!(10)],
        // 62 CopyArea: src(u32) dst(u32) gc(u32) src_x(i16) src_y(i16)
        //             dst_x(i16) dst_y(i16) w(u16) h(u16)
        62 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            i16f!(12),
            i16f!(14),
            i16f!(16),
            i16f!(18),
            u16f!(20),
            u16f!(22),
        ],
        // 63 CopyPlane: src(u32) dst(u32) gc(u32) src_x(i16) src_y(i16)
        //              dst_x(i16) dst_y(i16) w(u16) h(u16) bit_plane(u32)
        63 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            i16f!(12),
            i16f!(14),
            i16f!(16),
            i16f!(18),
            u16f!(20),
            u16f!(22),
            u32f!(24),
        ],
        // 64-71 Poly* drawing: drawable(u32) gc(u32) coords(i16/u16 array)
        64 | 65 | 66 | 67 | 70 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U16 }],
        // 68/71 PolyArc/PolyFillArc: drawable(u32) gc(u32) arcs(i16+u16+i16+i16 each)
        68 | 71 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U16 }],
        // 69 FillPoly: drawable(u32) gc(u32) shape/coords flags(u8 u8 u16) points(i16[])
        69 => &[
            u32f!(0),
            u32f!(4),
            u16f!(10),
            ElementArrayTail {
                from: 12,
                kind: I16,
            },
        ],
        // 72 PutImage: drawable(u32) gc(u32) w(u16) h(u16) dst_x(i16) dst_y(i16)
        //             [u8 left_pad u8 depth u16 pad] data(opaque)
        72 => &[
            u32f!(0),
            u32f!(4),
            u16f!(8),
            u16f!(10),
            i16f!(12),
            i16f!(14),
            OpaqueTail { from: 16 },
        ],
        // 73 GetImage: drawable(u32) x(i16) y(i16) w(u16) h(u16) plane_mask(u32)
        73 => &[u32f!(0), i16f!(4), i16f!(6), u16f!(8), u16f!(10), u32f!(12)],
        // 74 PolyText8 / 75 PolyText16 / 76 ImageText8 / 77 ImageText16:
        //   drawable(u32) gc(u32) x(i16) y(i16) text(opaque)
        74..=77 => &[
            u32f!(0),
            u32f!(4),
            i16f!(8),
            i16f!(10),
            OpaqueTail { from: 12 },
        ],
        // 78 CreateColormap: mid(u32) window(u32) visual(u32). alloc in header.data.
        78 => &[u32f!(0), u32f!(4), u32f!(8)],
        // 80 CopyColormapAndFree: mid(u32) src(u32)
        80 => &[u32f!(0), u32f!(4)],
        // 84 AllocColor: cmap(u32) red(u16) green(u16) blue(u16)
        84 => &[u32f!(0), u16f!(4), u16f!(6), u16f!(8)],
        // 85 AllocNamedColor: cmap(u32) name_len(u16) [u16 pad] name(opaque)
        85 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 86 AllocColorCells: cmap(u32) colors(u16) planes(u16). contiguous in header.data.
        86 => &[u32f!(0), u16f!(4), u16f!(6)],
        // 87 AllocColorPlanes: cmap(u32) colors(u16) reds(u16) greens(u16) blues(u16)
        87 => &[u32f!(0), u16f!(4), u16f!(6), u16f!(8), u16f!(10)],
        // 88 FreeColors: cmap(u32) plane_mask(u32) pixels(u32[])
        88 => &[u32f!(0), u32f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 89 StoreColors: cmap(u32) items(u32+u16+u16+u16+u8+pad each = 12 bytes)
        // Approximate: typed prefix u32, then a u16 array tail (under-swaps the u8s
        // but they're not sensitive).
        89 => &[u32f!(0), ElementArrayTail { from: 4, kind: U16 }],
        // 90 StoreNamedColor: cmap(u32) pixel(u32) name_len(u16) [u16 pad] name(opaque)
        // do_red/do_green/do_blue are bits in header.data.
        90 => &[u32f!(0), u32f!(4), u16f!(8), OpaqueTail { from: 12 }],
        // 91 QueryColors: cmap(u32) pixels(u32[])
        91 => &[u32f!(0), ElementArrayTail { from: 4, kind: U32 }],
        // 92 LookupColor: cmap(u32) name_len(u16) [u16 pad] name(opaque)
        92 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 93 CreateCursor: cid(u32) source(u32) mask(u32) fg/bg colors u16 x6
        //                 hot x(u16) hot y(u16)
        93 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            u16f!(22),
            u16f!(24),
            u16f!(26),
        ],
        // 94 CreateGlyphCursor: cid(u32) source_font(u32) mask_font(u32)
        //                      source_char(u16) mask_char(u16) fg/bg u16x6
        94 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u16f!(12),
            u16f!(14),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            u16f!(22),
            u16f!(24),
            u16f!(26),
        ],
        // 96 RecolorCursor: cursor(u32) fg/bg u16 x6
        96 => &[
            u32f!(0),
            u16f!(4),
            u16f!(6),
            u16f!(8),
            u16f!(10),
            u16f!(12),
            u16f!(14),
        ],
        // 97 QueryBestSize: drawable(u32) w(u16) h(u16). class in header.data.
        97 => &[u32f!(0), u16f!(4), u16f!(6)],
        // 98 QueryExtension: name_len(u16) [u16 pad] name(opaque)
        98 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 100 ChangeKeyboardMapping: keysym_per_keycode(u8 in header.data),
        //   first_keycode(u8 at body[0])? Actually:
        //   header.data = keycode_count, body[0] = first_keycode,
        //   body[1] = keysyms_per_keycode, body[2..4] = pad, body[4..] = u32[] keysyms
        100 => &[ElementArrayTail { from: 4, kind: U32 }],
        // 101 GetKeyboardMapping: first_keycode(u8) count(u8) [u16 pad]
        101 => &[],
        // 102 ChangeKeyboardControl: mask(u32) values(u32[])
        102 => &[u32f!(0), ElementArrayTail { from: 4, kind: U32 }],
        // 105 ChangePointerControl: accel_num(i16) accel_denom(i16) threshold(i16)
        //                           [u8 do_accel u8 do_threshold]
        105 => &[i16f!(0), i16f!(2), i16f!(4)],
        // 107 SetScreenSaver: timeout(i16) interval(i16) [u8 prefer_blanking u8 allow_exposures]
        107 => &[i16f!(0), i16f!(2)],
        // 109 ChangeHosts: family(u8 header.data), nbytes(u16 body[2..4])? Actually:
        //   mode(u8 header.data) family(u8 body[0]) [u8 pad] address_len(u16 body[2..4])
        //   address(opaque)
        109 => &[u16f!(2), OpaqueTail { from: 4 }],
        // 112 SetCloseDownMode: in header.data — body empty
        // 113 KillClient: resource(u32) — covered above
        // 114 RotateProperties: window(u32) num_props(u16) delta(i16) properties(u32[])
        114 => &[
            u32f!(0),
            u16f!(4),
            i16f!(6),
            ElementArrayTail { from: 8, kind: U32 },
        ],
        // 116 SetPointerMapping / 118 SetModifierMapping — body is u8[] (no swap)
        // 127 NoOperation — body opaque
        // Default: no entry.
        _ => return None,
    })
}

/// Per-minor swap table for the XInput extension (major opcode 137).
///
/// Field offsets are relative to the body (post-header). Every multi-
/// byte field that the matching handler reads (u16/u32/i16) needs an
/// entry; u8 fields and u8[] tails do not. Cross-referenced against
/// `XIproto.h` / `XI2proto.h` struct definitions.
#[allow(clippy::too_many_lines)]
const fn xi_request_swap_table(minor: u8) -> Option<&'static [FieldEntry]> {
    use FieldEntry::{ElementArrayTail, Fixed, OpaqueTail};
    use FieldKind::{U16, U32};

    macro_rules! u32f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U32,
            }
        };
    }
    macro_rules! u16f {
        ($off:expr) => {
            Fixed {
                offset: $off,
                kind: U16,
            }
        };
    }

    Some(match minor {
        // ── XI 1.x ─────────────────────────────────────────────────
        // 1 GetExtensionVersion: nbytes(u16) [u16 pad] name(opaque)
        1 => &[u16f!(0), OpaqueTail { from: 4 }],
        // 2 ListInputDevices: (no body fields)
        2 => &[],
        // 3 OpenDevice, 4 CloseDevice: deviceid(u8) — no u16/u32
        3 | 4 => &[],
        // 5 SetDeviceMode: deviceid(u8) mode(u8) [u16 pad]
        5 => &[],
        // 6 SelectExtensionEvent: window(u32) count(u16) [u16 pad] classes(u32[])
        6 => &[u32f!(0), u16f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 7 GetSelectedExtensionEvents: window(u32)
        7 => &[u32f!(0)],
        // 8 ChangeDeviceDontPropagateList: window(u32) count(u16)
        //   mode(u8) [u8 pad] classes(u32[])
        8 => &[u32f!(0), u16f!(4), ElementArrayTail { from: 8, kind: U32 }],
        // 9 GetDeviceDontPropagateList: window(u32)
        9 => &[u32f!(0)],
        // 10 GetDeviceMotionEvents: start(u32) stop(u32) device(u8) [u8 pad u16 pad]
        10 => &[u32f!(0), u32f!(4)],
        // 11 ChangeKeyboardDevice / 12 ChangePointerDevice: bytes only
        11 | 12 => &[],
        // 13 GrabDevice: window(u32) time(u32) count(u16) [u16 pad]
        //   this_mode(u8) other_mode(u8) owner_events(u8) device(u8)
        //   classes(u32[])
        13 => &[
            u32f!(0),
            u32f!(4),
            u16f!(8),
            ElementArrayTail {
                from: 16,
                kind: U32,
            },
        ],
        // 14 UngrabDevice: time(u32) device(u8) [u8 pad u16 pad]
        14 => &[u32f!(0)],
        // 15 GrabDeviceKey: window(u32) count(u16) modifiers(u16)
        //   modifier_device(u8) grabbed_device(u8) key(u8) this_mode(u8)
        //   other_mode(u8) owner_events(u8) [u16 pad] classes(u32[])
        15 => &[
            u32f!(0),
            u16f!(4),
            u16f!(6),
            ElementArrayTail {
                from: 16,
                kind: U32,
            },
        ],
        // 16 UngrabDeviceKey: window(u32) modifiers(u16) modifier_device(u8)
        //   key(u8) grabbed_device(u8) [u8 pad u16 pad]
        16 => &[u32f!(0), u16f!(4)],
        // 17 GrabDeviceButton: window(u32) grabbed_device(u8) modifier_device(u8)
        //   event_count(u16) modifiers(u16) this_mode(u8) other_mode(u8)
        //   button(u8) ownerEvents(u8) [u16 pad] classes(u32[])
        17 => &[
            u32f!(0),
            u16f!(6),
            u16f!(8),
            ElementArrayTail {
                from: 16,
                kind: U32,
            },
        ],
        // 18 UngrabDeviceButton: window(u32) modifiers(u16) modifier_device(u8)
        //   button(u8) grabbed_device(u8) [u8 pad u16 pad]
        18 => &[u32f!(0), u16f!(4)],
        // 19 AllowDeviceEvents: time(u32) mode(u8) deviceid(u8) [u16 pad]
        19 => &[u32f!(0)],
        // 20 GetDeviceFocus: device(u8) [u8 pad u16 pad]
        20 => &[],
        // 21 SetDeviceFocus: focus(u32) time(u32) revertTo(u8) device(u8) [u16 pad]
        21 => &[u32f!(0), u32f!(4)],
        // 22 GetFeedbackControl: device(u8) [u8 pad u16 pad]
        22 => &[],
        // 23 ChangeFeedbackControl: mask(u32) device(u8) feedbackid(u8)
        //   [u16 pad] feedback(tagged union, swapped dynamically above).
        23 => &[u32f!(0)],
        // 24 GetDeviceKeyMapping: device(u8) first_keycode(u8) count(u8) [u8 pad]
        24 => &[],
        // 25 ChangeDeviceKeyMapping: device(u8) first_keycode(u8)
        //   keysyms_per_keycode(u8) keycode_count(u8) keysyms(u32[])
        25 => &[ElementArrayTail { from: 4, kind: U32 }],
        // 26 GetDeviceModifierMapping: device(u8) [u8 pad u16 pad]
        26 => &[],
        // 27 SetDeviceModifierMapping: device(u8) keycodes_per_modifier(u8)
        //   [u16 pad] keycodes(u8[])
        27 => &[],
        // 28 GetDeviceButtonMapping: device(u8) [u8 pad u16 pad]
        28 => &[],
        // 29 SetDeviceButtonMapping: device(u8) map_length(u8) [u16 pad] map(u8[])
        29 => &[],
        // 30 QueryDeviceState: device(u8) [u8 pad u16 pad]
        30 => &[],
        // 31 SendExtensionEvent: destination(u32) device(u8) propagate(u8)
        //   count(u16) num_events(u8) [u8 pad u16 pad] events(opaque×num_events)
        //   classes(u32[]). The synthetic event payload is the *sender's*
        //   byte order (just like core SendEvent); leave as opaque for now.
        31 => &[u32f!(0), u16f!(6), OpaqueTail { from: 12 }],
        // 32 DeviceBell: device(u8) feedback_class(u8) feedback_id(u8) percent(i8)
        32 => &[],
        // 33 SetDeviceValuators: device(u8) first_valuator(u8) num_valuators(u8)
        //   [u8 pad] valuators(u32[])
        33 => &[ElementArrayTail { from: 4, kind: U32 }],
        // 34 GetDeviceControl: control(u16) device(u8) [u8 pad]
        34 => &[u16f!(0)],
        // 35 ChangeDeviceControl: outer control here; the embedded control
        // header and resolution array are swapped dynamically above.
        35 => &[u16f!(0)],
        // 36 ListDeviceProperties: device(u8) [u8 pad u16 pad]
        36 => &[],
        // 37 ChangeDeviceProperty: property(u32) type(u32) device(u8)
        //   format(u8) mode(u8) [u8 pad] num_items(u32) value(format-typed)
        37 => &[u32f!(0), u32f!(4), u32f!(12), OpaqueTail { from: 16 }],
        // 38 DeleteDeviceProperty: property(u32) device(u8) [u8 pad u16 pad]
        38 => &[u32f!(0)],
        // 39 GetDeviceProperty: property(u32) type(u32) longOffset(u32)
        //   longLength(u32) device(u8) delete(u8) [u16 pad]
        39 => &[u32f!(0), u32f!(4), u32f!(8), u32f!(12)],
        // ── XI 2 ───────────────────────────────────────────────────
        // 40 XIQueryPointer: window(u32) deviceid(u16) [u16 pad]
        40 => &[u32f!(0), u16f!(4)],
        // 41 XIWarpPointer: src_win(u32) dst_win(u32) src_x(i32-fp1616)
        //   src_y(i32-fp1616) src_w(u16) src_h(u16) dst_x(i32-fp1616)
        //   dst_y(i32-fp1616) deviceid(u16) [u16 pad]. The fp1616 fields
        //   are u32-wide, swap as u32.
        41 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u16f!(16),
            u16f!(18),
            u32f!(20),
            u32f!(24),
            u16f!(28),
        ],
        // 42 XIChangeCursor: window(u32) cursor(u32) deviceid(u16) [u16 pad]
        42 => &[u32f!(0), u32f!(4), u16f!(8)],
        // 43 XIChangeHierarchy: num_changes(u8) [u8 pad u16 pad] changes(opaque)
        43 => &[OpaqueTail { from: 4 }],
        // 44 XISetClientPointer: window(u32) deviceid(u16) [u16 pad]
        44 => &[u32f!(0), u16f!(4)],
        // 45 XIGetClientPointer: window(u32)
        45 => &[u32f!(0)],
        // 46 XISelectEvents: window(u32) num_masks(u16) [u16 pad]
        //   masks(opaque event-mask records, each: deviceid(u16) mask_len(u16)
        //   mask(u8 × pad32(mask_len)))
        46 => &[u32f!(0), u16f!(4), OpaqueTail { from: 8 }],
        // 47 XIQueryVersion: major(u16) minor(u16)
        47 => &[u16f!(0), u16f!(2)],
        // 48 XIQueryDevice: deviceid(u16) [u16 pad]
        48 => &[u16f!(0)],
        // 49 XISetFocus: window(u32) time(u32) deviceid(u16) [u16 pad]
        49 => &[u32f!(0), u32f!(4), u16f!(8)],
        // 50 XIGetFocus: deviceid(u16) [u16 pad]
        50 => &[u16f!(0)],
        // 51 XIGrabDevice: window(u32) time(u32) cursor(u32) deviceid(u16)
        //   mode(u8) paired_device_mode(u8) owner_events(u8) [u8 pad]
        //   mask_len(u16) mask(u8 × pad32(mask_len))
        51 => &[u32f!(0), u32f!(4), u32f!(8), u16f!(12), u16f!(18)],
        // 52 XIUngrabDevice: time(u32) deviceid(u16) [u16 pad]
        52 => &[u32f!(0), u16f!(4)],
        // 53 XIAllowEvents: time(u32) deviceid(u16) event_mode(u8) [u8 pad]
        //   touchid(u32 — XI 2.2+ only) grab_window(u32 — XI 2.2+ only)
        53 => &[u32f!(0), u16f!(4), u32f!(8), u32f!(12)],
        // 54 XIPassiveGrabDevice: time(u32) grab_window(u32) cursor(u32)
        //   detail(u32) deviceid(u16) num_modifiers(u16) mask_len(u16)
        //   grab_type(u8) grab_mode(u8) paired_device_mode(u8) owner_events(u8)
        //   [u16 pad] mask(u8 × pad32(mask_len)) modifiers(u32 × num_modifiers)
        54 => &[
            u32f!(0),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            u16f!(16),
            u16f!(18),
            u16f!(20),
            OpaqueTail { from: 28 },
        ],
        // 55 XIPassiveUngrabDevice: grab_window(u32) detail(u32) deviceid(u16)
        //   num_modifiers(u16) grab_type(u8) [u8 pad u16 pad] modifiers(u32[])
        55 => &[
            u32f!(0),
            u32f!(4),
            u16f!(8),
            u16f!(10),
            ElementArrayTail {
                from: 16,
                kind: U32,
            },
        ],
        // 56 XIListProperties: deviceid(u16) [u16 pad]
        56 => &[u16f!(0)],
        // 57 XIChangeProperty: deviceid(u16) mode(u8) format(u8) property(u32)
        //   type(u32) num_items(u32) value(format-typed)
        57 => &[
            u16f!(0),
            u32f!(4),
            u32f!(8),
            u32f!(12),
            OpaqueTail { from: 16 },
        ],
        // 58 XIDeleteProperty: deviceid(u16) [u16 pad] property(u32)
        58 => &[u16f!(0), u32f!(4)],
        // 59 XIGetProperty: deviceid(u16) delete(u8) [u8 pad] property(u32)
        //   type(u32) offset(u32) len(u32)
        59 => &[u16f!(0), u32f!(4), u32f!(8), u32f!(12), u32f!(16)],
        // 60 XIGetSelectedEvents: window(u32)
        60 => &[u32f!(0)],
        // 61 XIBarrierReleasePointer: num_barriers(u32) records(deviceid u16
        //   pad u16 barrier u32 eventid u32 — 12 bytes each)
        61 => &[u32f!(0), ElementArrayTail { from: 4, kind: U32 }],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_4_destroy_window_swaps_window_id() {
        let mut body = vec![0xaa, 0xbb, 0xcc, 0xdd];
        swap_request_body(4, 0, ClientByteOrder::BigEndian, &mut body);
        // u32 0xaabbccdd in BE → bytes [0xaa, 0xbb, 0xcc, 0xdd]; native LE
        // representation after swap → [0xdd, 0xcc, 0xbb, 0xaa].
        assert_eq!(body, vec![0xdd, 0xcc, 0xbb, 0xaa]);
    }

    #[test]
    fn opcode_84_alloc_color_swaps_cmap_and_color_components() {
        // body: cmap(u32) red(u16) green(u16) blue(u16) [u16 pad]
        let mut body = vec![
            0x00, 0x00, 0x00, 0x10, // cmap = 0x10 in BE
            0x00, 0xff, // red = 0xff in BE
            0x00, 0xaa, // green = 0xaa
            0x00, 0x55, // blue = 0x55
            0x00, 0x00, // pad
        ];
        swap_request_body(84, 0, ClientByteOrder::BigEndian, &mut body);
        // After swap the body is in LE, decoded values stay the same.
        assert_eq!(
            u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            0x10
        );
        assert_eq!(u16::from_le_bytes([body[4], body[5]]), 0xff);
        assert_eq!(u16::from_le_bytes([body[6], body[7]]), 0xaa);
        assert_eq!(u16::from_le_bytes([body[8], body[9]]), 0x55);
    }

    #[test]
    fn unknown_opcode_passes_through() {
        let mut body = vec![1, 2, 3, 4];
        let original = body.clone();
        // 200 is well beyond core + extension space.
        swap_request_body(200, 0, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn le_client_no_op() {
        let mut body = vec![0xaa, 0xbb, 0xcc, 0xdd];
        let original = body.clone();
        swap_request_body(4, 0, ClientByteOrder::LittleEndian, &mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn xi_grab_device_button_swaps_window_field() {
        // XI GrabDeviceButton body (after 4-byte header):
        //   window(u32) grabbed_device(u8) modifier_device(u8)
        //   event_count(u16) modifiers(u16) ...
        let mut body = vec![
            0x00, 0x60, 0x00, 0x00, // window = 0x00600000 in BE
            4,    // grabbed_device
            0,    // modifier_device
            0x00, 0x01, // event_count = 1 in BE
            0xff, 0xff, // modifiers = 0xffff (AnyModifier) — symmetric, but exercise swap
            1, 1, 0, 1, // this_mode=Async other_mode=Async button=AnyButton owner=True
            0, 0, // pad
        ];
        // XInputExtension major opcode = 137; X_GrabDeviceButton minor = 17.
        swap_request_body(137, 17, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(
            u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
            0x00600000,
            "window field must be swapped from BE to LE",
        );
        assert_eq!(u16::from_le_bytes([body[6], body[7]]), 1, "event_count");
    }

    #[test]
    fn xi_change_feedback_control_swaps_tagged_keyboard_payload() {
        let mut body = vec![0; 28];
        body[0..4].copy_from_slice(&0x1fu32.to_be_bytes());
        body[4] = 3;
        body[5] = 0; // KbdFeedbackClass
        body[8] = 0;
        body[9] = 0;
        body[10..12].copy_from_slice(&20u16.to_be_bytes());
        body[16..18].copy_from_slice(&400i16.to_be_bytes());
        body[18..20].copy_from_slice(&100i16.to_be_bytes());
        body[20..24].copy_from_slice(&3u32.to_be_bytes());
        body[24..28].copy_from_slice(&2u32.to_be_bytes());

        swap_request_body(137, 23, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 0x1f);
        assert_eq!(u16::from_le_bytes(body[10..12].try_into().unwrap()), 20);
        assert_eq!(i16::from_le_bytes(body[16..18].try_into().unwrap()), 400);
        assert_eq!(i16::from_le_bytes(body[18..20].try_into().unwrap()), 100);
        assert_eq!(u32::from_le_bytes(body[20..24].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(body[24..28].try_into().unwrap()), 2);
    }

    #[test]
    fn xi_change_device_control_swaps_resolution_payload() {
        let mut body = vec![0; 20];
        body[0..2].copy_from_slice(&1u16.to_be_bytes());
        body[2] = 2;
        body[4..6].copy_from_slice(&1u16.to_be_bytes());
        body[6..8].copy_from_slice(&16u16.to_be_bytes());
        body[8] = 0;
        body[9] = 2;
        body[12..16].copy_from_slice(&1000u32.to_be_bytes());
        body[16..20].copy_from_slice(&2000u32.to_be_bytes());

        swap_request_body(137, 35, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(u16::from_le_bytes(body[0..2].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(body[4..6].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(body[6..8].try_into().unwrap()), 16);
        assert_eq!(u32::from_le_bytes(body[12..16].try_into().unwrap()), 1000);
        assert_eq!(u32::from_le_bytes(body[16..20].try_into().unwrap()), 2000);
    }

    #[test]
    fn xi_extension_unknown_minor_passes_through() {
        // major=137 is XI, but minor=255 has no swap entry → no-op.
        let mut body = vec![0xaa, 0xbb, 0xcc, 0xdd];
        let original = body.clone();
        swap_request_body(137, 255, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn xf86vidmode_read_requests_swap_screen_and_gamma_ramp_size() {
        for minor in [1, 4, 6, 11, 13, 16, 19, 20] {
            let mut body = vec![0x12, 0x34, 0, 0];
            if minor == 16 {
                body.resize(28, 0);
            }
            swap_request_body(153, minor, ClientByteOrder::BigEndian, &mut body);
            assert_eq!(
                u16::from_le_bytes(body[0..2].try_into().unwrap()),
                0x1234,
                "minor {minor}"
            );
        }

        let mut ramp = vec![0x12, 0x34, 0x01, 0x00];
        swap_request_body(153, 17, ClientByteOrder::BigEndian, &mut ramp);
        assert_eq!(u16::from_le_bytes(ramp[0..2].try_into().unwrap()), 0x1234);
        assert_eq!(u16::from_le_bytes(ramp[2..4].try_into().unwrap()), 0x0100);
    }

    #[test]
    fn xf86vidmode_validate_body_stays_in_client_byte_order() {
        let mut body = vec![0u8; 48];
        body[0..4].copy_from_slice(&1u32.to_be_bytes());
        body[44..48].copy_from_slice(&0u32.to_be_bytes());
        let original = body.clone();
        swap_request_body(153, 9, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn xf86vidmode_get_mode_line_and_client_version_are_swapped() {
        let mut screen = [0x12, 0x34, 0, 0];
        swap_request_body(153, 1, ClientByteOrder::BigEndian, &mut screen);
        assert_eq!(screen, [0x34, 0x12, 0, 0]);

        let mut version = [0, 2, 0, 1];
        swap_request_body(153, 14, ClientByteOrder::BigEndian, &mut version);
        assert_eq!(version, [2, 0, 1, 0]);
    }

    #[test]
    fn every_screen_taking_xf86vidmode_request_swaps_its_screen_field() {
        // GetAllModeLines, GetGammaRampSize, GetPermissions share
        // GetModeLine's `screen(u16) + pad(u16)` body.
        for minor in [1, 6, 19, 20] {
            let mut body = [0x12, 0x34, 0, 0];
            swap_request_body(153, minor, ClientByteOrder::BigEndian, &mut body);
            assert_eq!(body, [0x34, 0x12, 0, 0], "minor {minor}");
        }
    }

    #[test]
    fn xf86vidmode_little_endian_client_body_is_untouched() {
        let mut body = [0x12, 0x34, 0, 0];
        swap_request_body(153, 1, ClientByteOrder::LittleEndian, &mut body);
        assert_eq!(body, [0x12, 0x34, 0, 0]);
    }

    #[test]
    fn non_xi_extension_passes_through() {
        // major=140 (XFIXES) currently has no swap table → no-op.
        let mut body = vec![0xaa, 0xbb, 0xcc, 0xdd];
        let original = body.clone();
        swap_request_body(140, 0, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn randr_set_crtc_gamma_swaps_crtc_size_and_tail() {
        let mut body = vec![
            0x00, 0x00, 0x00, 0x02, // crtc = 2 in BE
            0x00, 0x02, // size = 2 in BE
            0x00, 0x00, // pad
            0x00, 0x01, 0x00, 0x02, // red
            0x00, 0x03, 0x00, 0x04, // green
            0x00, 0x05, 0x00, 0x06, // blue
        ];
        swap_request_body(
            128,
            crate::x11::randr::RR_SET_CRTC_GAMMA,
            ClientByteOrder::BigEndian,
            &mut body,
        );
        assert_eq!(u32::from_le_bytes([body[0], body[1], body[2], body[3]]), 2);
        assert_eq!(u16::from_le_bytes([body[4], body[5]]), 2);
        assert_eq!(u16::from_le_bytes([body[8], body[9]]), 1);
        assert_eq!(u16::from_le_bytes([body[18], body[19]]), 6);
    }

    #[test]
    fn randr_configure_output_property_swaps_header_and_tail_int32_array() {
        let mut body = vec![
            0x00, 0x00, 0x00, 0x02, // output = 2 in BE
            0x00, 0x00, 0x00, 0x03, // property = 3 in BE
            1, 0, 0, 0, // pending=true, range=false, pad
            0x00, 0x00, 0x00, 0x0a, // valid_values[0] = 10 in BE
            0x00, 0x00, 0x00, 0x14, // valid_values[1] = 20 in BE
        ];
        swap_request_body(128, 12, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(body[12..16].try_into().unwrap()), 10);
        assert_eq!(u32::from_le_bytes(body[16..20].try_into().unwrap()), 20);
    }

    #[test]
    fn randr_change_output_property_swaps_tail_by_format32() {
        let mut body = vec![
            0x00, 0x00, 0x00, 0x02, // output
            0x00, 0x00, 0x00, 0x03, // property
            0x00, 0x00, 0x00, 0x04, // type
            32, 0, 0, 0, // format=32, mode, pad
            0x00, 0x00, 0x00, 0x02, // nUnits = 2
            0x00, 0x00, 0x00, 0x0a, // value[0] = 10 in BE
            0x00, 0x00, 0x00, 0x14, // value[1] = 20 in BE
        ];
        swap_request_body(128, 13, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 4);
        assert_eq!(u32::from_le_bytes(body[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(body[20..24].try_into().unwrap()), 10);
        assert_eq!(u32::from_le_bytes(body[24..28].try_into().unwrap()), 20);
    }

    #[test]
    fn randr_change_output_property_swaps_tail_by_format16() {
        let mut body = vec![
            0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04, 16, 0, 0,
            0, // format=16
            0x00, 0x00, 0x00, 0x02, // nUnits = 2
            0x00, 0x0a, // value[0] = 10 in BE
            0x00, 0x14, // value[1] = 20 in BE
        ];
        swap_request_body(128, 13, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(u16::from_le_bytes(body[20..22].try_into().unwrap()), 10);
        assert_eq!(u16::from_le_bytes(body[22..24].try_into().unwrap()), 20);
    }

    #[test]
    fn randr_change_output_property_format8_tail_untouched() {
        let mut body = vec![
            0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04, 8, 0, 0,
            0, // format=8
            0x00, 0x00, 0x00, 0x02, // nUnits = 2
            0xAB, 0xCD, // opaque byte-format data
        ];
        let original_tail = body[20..22].to_vec();
        swap_request_body(128, 13, ClientByteOrder::BigEndian, &mut body);
        assert_eq!(body[20..22], original_tail[..]);
    }

    /// Every RANDR minor whose handler reads a resource id must swap it. Only
    /// SetCrtcGamma had an entry, so a big-endian client's xid was read back
    /// byte-reversed — which the resource validation then rejected as a bogus
    /// BadWindow/BadOutput/BadCrtc. Not hypothetical: X11 is network
    /// transparent, so `ssh -X` from a big-endian host (AIX/POWER, s390x,
    /// Solaris/SPARC) drives a big-endian client against this server.
    #[test]
    fn randr_leading_resource_id_is_swapped_for_every_validating_minor() {
        const XID: u32 = 0x00de_ad01;
        for minor in [
            2u8, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 20, 21, 22, 23, 25, 27, 28, 30, 31, 32,
            33, 42, 46,
        ] {
            let mut body = vec![0u8; 32];
            body[0..4].copy_from_slice(&XID.to_be_bytes());
            swap_request_body(128, minor, ClientByteOrder::BigEndian, &mut body);
            assert_eq!(
                u32::from_le_bytes(body[0..4].try_into().unwrap()),
                XID,
                "minor {minor} must swap its leading resource id",
            );
        }
    }

    /// A little-endian client's body must be left byte-for-byte alone, so the
    /// new entries cannot corrupt the common path.
    #[test]
    fn randr_little_endian_bodies_are_untouched() {
        for minor in [2u8, 4, 7, 12, 13, 15, 21, 30, 42, 46] {
            let mut body: Vec<u8> = (0..32u8).collect();
            let original = body.clone();
            swap_request_body(128, minor, ClientByteOrder::LittleEndian, &mut body);
            assert_eq!(body, original, "minor {minor} must not touch LE bodies");
        }
    }
}
