//! The X cursor as the window's cursor: images become alpha cursors
//! (`CreateIconIndirect` on a 32-bit DIB) of `factor`x`factor` device
//! pixels per X pixel, cached by image version and factor, shown by
//! `WM_SETCURSOR` over the client area. UI thread only.

use std::{cell::RefCell, collections::HashMap, ptr};

use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::{
        BI_BITFIELDS, BITMAPINFO, BITMAPV5HEADER, ClientToScreen, CreateBitmap, CreateDIBSection,
        DIB_RGB_COLORS, DeleteObject, GetDC, ReleaseDC, ScreenToClient,
    },
    UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, GetCursorPos, GetForegroundWindow, HCURSOR, ICONINFO,
        SetCursor, SetCursorPos, WindowFromPoint,
    },
};

use crate::surface::ui::CursorImage;

/// Cursors kept before the cache starts over.
const CACHE_LIMIT: usize = 64;

#[derive(Default)]
struct Cursors {
    /// By image version, hot spot and factor; values are owned.
    by_image: HashMap<(u64, (u16, u16), u32), HCURSOR>,
    /// What the client area shows: an image's cursor, or none.
    current: HCURSOR,
    /// Its image, to build again at another factor.
    image: Option<CursorImage>,
    /// Device pixels per X pixel per axis (0 until set: 1).
    factor: u32,
}

thread_local! {
    static CURSORS: RefCell<Cursors> = RefCell::new(Cursors::default());
}

/// Show `image` over `hwnd`'s client area from now on, or no cursor.
pub(super) fn set(hwnd: HWND, image: Option<&CursorImage>) {
    let cursor = select(image);
    if pointer_inside(hwnd) {
        // SAFETY: a live cursor of this cache, or null (none).
        unsafe { SetCursor(cursor) };
    }
}

/// Rootless: show `image` (`None`: no cursor) over every top-level's
/// client area; at once when the pointer is `inside` one of them, else at
/// its next `WM_SETCURSOR`.
pub(super) fn set_global(image: Option<&CursorImage>, inside: bool) {
    let cursor = select(image);
    if inside {
        // SAFETY: as in `set`.
        unsafe { SetCursor(cursor) };
    }
}

/// Show X cursors at `factor` device pixels per X pixel from now on; the
/// current one at once when the pointer is `inside` the client area.
pub(super) fn set_factor(factor: u32, inside: bool) {
    let image = CURSORS.with_borrow_mut(|cursors| {
        if cursors.factor.max(1) == factor.max(1) {
            return None;
        }
        cursors.factor = factor.max(1);
        Some(cursors.image.clone())
    });
    if let Some(image) = image {
        let cursor = select(image.as_ref());
        if inside {
            // SAFETY: as in `set`.
            unsafe { SetCursor(cursor) };
        }
    }
}

/// Make `image`'s cursor the current one.
fn select(image: Option<&CursorImage>) -> HCURSOR {
    CURSORS.with_borrow_mut(|cursors| {
        let factor = cursors.factor.max(1);
        let cursor = match image {
            Some(image) => {
                let key = (image.version, image.hot, factor);
                if let Some(&cursor) = cursors.by_image.get(&key) {
                    cursor
                } else {
                    if cursors.by_image.len() >= CACHE_LIMIT {
                        let current = cursors.current;
                        cursors.by_image.retain(|_, &mut cursor| {
                            if cursor != current {
                                // SAFETY: a cursor this cache created and
                                // nobody shows.
                                unsafe { DestroyIcon(cursor) };
                            }
                            cursor == current
                        });
                    }
                    let cursor = build(image, factor);
                    if !cursor.is_null() {
                        cursors.by_image.insert(key, cursor);
                    }
                    cursor
                }
            }
            None => ptr::null_mut(),
        };
        cursors.current = cursor;
        cursors.image = image.cloned();
        cursor
    })
}

/// `WM_SETCURSOR` over the client area: show the X cursor.
pub(super) fn apply_current() {
    let cursor = CURSORS.with_borrow(|cursors| cursors.current);
    // SAFETY: as in `set`.
    unsafe { SetCursor(cursor) };
}

/// Move the host pointer to X pixel `(x, y)` of `hwnd`'s client area,
/// while the window is the foreground one and the pointer is over it, as
/// on macOS.
pub(super) fn warp(hwnd: HWND, x: i32, y: i32) {
    // SAFETY: a plain query.
    if unsafe { GetForegroundWindow() } != hwnd || !pointer_inside(hwnd) {
        return;
    }
    let factor = i32::try_from(CURSORS.with_borrow(|c| c.factor.max(1))).unwrap_or(1);
    let mut at = POINT {
        x: x.saturating_mul(factor),
        y: y.saturating_mul(factor),
    };
    // SAFETY: a live window; writes `at`.
    unsafe {
        ClientToScreen(hwnd, &raw mut at);
        SetCursorPos(at.x, at.y);
    }
}

/// Rootless: move the host pointer to screen pixel `(x, y)`; no "inside"
/// guard (spec §5 Global).
pub(super) fn warp_screen(x: i32, y: i32) {
    // SAFETY: a plain call.
    unsafe { SetCursorPos(x, y) };
}

/// Whether the host pointer is over `hwnd`'s client area, nothing above it.
pub(super) fn pointer_inside(hwnd: HWND) -> bool {
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: writes `at`; plain queries.
    if unsafe { GetCursorPos(&raw mut at) == 0 || WindowFromPoint(at) != hwnd } {
        return false;
    }
    // SAFETY: a live window; writes `at`.
    unsafe { ScreenToClient(hwnd, &raw mut at) };
    let (width, height) = super::window::client_size(hwnd);
    (0..width).contains(&at.x) && (0..height).contains(&at.y)
}

/// An alpha cursor of `image`, each pixel `factor`x`factor`; null when GDI
/// refuses.
fn build(image: &CursorImage, factor: u32) -> HCURSOR {
    let factor = factor.clamp(1, 8);
    let (src_w, src_h) = (image.width.max(1), image.height.max(1));
    let (width, height) = (src_w * factor, src_h * factor);
    let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
        return ptr::null_mut();
    };
    // SAFETY: zeroed is a valid header; the fields that matter are set.
    let mut header: BITMAPV5HEADER = unsafe { std::mem::zeroed() };
    header.bV5Size = u32::try_from(size_of::<BITMAPV5HEADER>()).unwrap_or(0);
    header.bV5Width = w;
    // Negative: rows top-down, as the image's.
    header.bV5Height = -h;
    header.bV5Planes = 1;
    header.bV5BitCount = 32;
    header.bV5Compression = BI_BITFIELDS;
    header.bV5RedMask = 0x00FF_0000;
    header.bV5GreenMask = 0x0000_FF00;
    header.bV5BlueMask = 0x0000_00FF;
    header.bV5AlphaMask = 0xFF00_0000;
    let mut bits: *mut core::ffi::c_void = ptr::null_mut();
    // SAFETY: the screen DC, released below; a V5 header is a valid
    // BITMAPINFO prefix; writes `bits`.
    let color = unsafe {
        let dc = GetDC(ptr::null_mut());
        let color = CreateDIBSection(
            dc,
            (&raw const header).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
            &raw mut bits,
            ptr::null_mut(),
            0,
        );
        ReleaseDC(ptr::null_mut(), dc);
        color
    };
    if color.is_null() || bits.is_null() {
        return ptr::null_mut();
    }
    let len = width as usize * height as usize * 4;
    // SAFETY: the section holds `width * height` 32-bit pixels.
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), len) };
    let src_len = src_w as usize * src_h as usize * 4;
    if image.bgra.len() >= src_len {
        scale_nearest(
            &image.bgra[..src_len],
            src_w as usize,
            factor as usize,
            pixels,
        );
    }
    // Icons take straight alpha; X cursors are premultiplied.
    for px in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(px[3]);
        if alpha != 0 && alpha != 255 {
            for c in &mut px[..3] {
                *c = u8::try_from((u32::from(*c) * 255 + alpha / 2) / alpha).unwrap_or(255);
            }
        }
    }
    // SAFETY: a 1-bpp mask, unused with an alpha color bitmap but required.
    let mask = unsafe { CreateBitmap(w, h, 1, 1, ptr::null()) };
    let info = ICONINFO {
        fIcon: 0,
        xHotspot: u32::from(image.hot.0) * factor,
        yHotspot: u32::from(image.hot.1) * factor,
        hbmMask: mask,
        hbmColor: color,
    };
    // SAFETY: both bitmaps are live; the cursor copies them, so they go.
    unsafe {
        let cursor = CreateIconIndirect(&raw const info);
        DeleteObject(color);
        if !mask.is_null() {
            DeleteObject(mask);
        }
        cursor
    }
}

/// `src` (`width` 4-byte pixels a row) into `dst`, each pixel `factor` x
/// `factor` times.
fn scale_nearest(src: &[u8], width: usize, factor: usize, dst: &mut [u8]) {
    if factor == 1 {
        dst.copy_from_slice(src);
        return;
    }
    let out_row = width * factor * 4;
    for (y, row) in src.chunks_exact(width * 4).enumerate() {
        let first = y * factor * out_row;
        for (x, px) in row.chunks_exact(4).enumerate() {
            for k in 0..factor {
                let at = first + (x * factor + k) * 4;
                dst[at..at + 4].copy_from_slice(px);
            }
        }
        for k in 1..factor {
            dst.copy_within(first..first + out_row, first + k * out_row);
        }
    }
}
