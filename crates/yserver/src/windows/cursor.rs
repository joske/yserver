//! The X cursor as the window's cursor: images become alpha cursors
//! (`CreateIconIndirect` on a 32-bit DIB), cached by image version, shown
//! by `WM_SETCURSOR` over the client area. UI thread only.

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
    /// By image version and hot spot; values are owned.
    by_image: HashMap<(u64, (u16, u16)), HCURSOR>,
    /// What the client area shows: an image's cursor, or none.
    current: HCURSOR,
}

thread_local! {
    static CURSORS: RefCell<Cursors> = RefCell::new(Cursors::default());
}

/// Show `image` over `hwnd`'s client area from now on, or no cursor.
pub(super) fn set(hwnd: HWND, image: Option<&CursorImage>) {
    let cursor = CURSORS.with_borrow_mut(|cursors| {
        let cursor = match image {
            Some(image) => {
                let key = (image.version, image.hot);
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
                    let cursor = build(image);
                    if !cursor.is_null() {
                        cursors.by_image.insert(key, cursor);
                    }
                    cursor
                }
            }
            None => ptr::null_mut(),
        };
        cursors.current = cursor;
        cursor
    });
    if pointer_inside(hwnd) {
        // SAFETY: a live cursor of this cache, or null (none).
        unsafe { SetCursor(cursor) };
    }
}

/// `WM_SETCURSOR` over the client area: show the X cursor.
pub(super) fn apply_current() {
    let cursor = CURSORS.with_borrow(|cursors| cursors.current);
    // SAFETY: as in `set`.
    unsafe { SetCursor(cursor) };
}

/// Move the host pointer to client pixel `(x, y)` of `hwnd`, while the
/// window is the foreground one and the pointer is over it, as on macOS.
pub(super) fn warp(hwnd: HWND, x: i32, y: i32) {
    // SAFETY: a plain query.
    if unsafe { GetForegroundWindow() } != hwnd || !pointer_inside(hwnd) {
        return;
    }
    let mut at = POINT { x, y };
    // SAFETY: a live window; writes `at`.
    unsafe {
        ClientToScreen(hwnd, &raw mut at);
        SetCursorPos(at.x, at.y);
    }
}

/// Whether the host pointer is over `hwnd`'s client area, nothing above it.
fn pointer_inside(hwnd: HWND) -> bool {
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: writes `at`; plain queries.
    if unsafe { GetCursorPos(&raw mut at) == 0 || WindowFromPoint(at) != hwnd } {
        return false;
    }
    // SAFETY: a live window; writes `at`.
    unsafe { ScreenToClient(hwnd, &raw mut at) };
    let (width, height) = super::window::client_size(hwnd);
    (0..i32::from(width)).contains(&at.x) && (0..i32::from(height)).contains(&at.y)
}

/// An alpha cursor of `image`; null when GDI refuses.
fn build(image: &CursorImage) -> HCURSOR {
    let (width, height) = (image.width.max(1), image.height.max(1));
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
    if image.bgra.len() >= len {
        pixels.copy_from_slice(&image.bgra[..len]);
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
        xHotspot: u32::from(image.hot.0),
        yHotspot: u32::from(image.hot.1),
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
