//! The window: a resizable top-level whose client area is the X screen, one
//! window pixel per X pixel, and the Vulkan surface on it.

use std::{io, ptr};

use ash::vk;
use windows_sys::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::{GetDC, GetDeviceCaps, HORZRES, HORZSIZE, ReleaseDC, VERTRES, VERTSIZE},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
            GetClientRect, IDC_ARROW, LoadCursorW, RegisterClassExW, SPI_GETWORKAREA,
            SetWindowTextW, SystemParametersInfoW, WINDOW_EX_STYLE, WNDCLASSEXW, WNDPROC,
            WS_OVERLAPPEDWINDOW,
        },
    },
    core::w,
};

use crate::surface::{
    resize::{self, Size, to_px},
    swapchain::NativeSurface,
};

const CLASS_NAME: windows_sys::core::PCWSTR = w!("yserver");
const STYLE: u32 = WS_OVERLAPPEDWINDOW;
const EX_STYLE: WINDOW_EX_STYLE = 0;

/// A titled, closable, minimisable, resizable window whose client area is
/// `size` X pixels, not yet shown; `wndproc` handles its messages.
///
/// # Errors
///
/// The class or the window could not be created.
pub(super) fn create(size: Size, wndproc: WNDPROC) -> io::Result<HWND> {
    // SAFETY: the module handle of this executable.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class = WNDCLASSEXW {
        cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: wndproc,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: super::icon::large(),
        // SAFETY: a system resource by predefined id.
        hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) },
        // No background brush: Vulkan paints every pixel.
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: CLASS_NAME,
        hIconSm: super::icon::small(),
    };
    // SAFETY: a fully initialised class description.
    if unsafe { RegisterClassExW(&raw const class) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (width, height) = frame_size(size);
    // SAFETY: a registered class; no parent, menu or creation data.
    let hwnd = unsafe {
        CreateWindowExW(
            EX_STYLE,
            CLASS_NAME,
            w!("yserver"),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(hwnd)
}

/// The outer size of a window whose client area is `size`.
pub(super) fn frame_size(size: Size) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: i32::from(size.0),
        bottom: i32::from(size.1),
    };
    // SAFETY: writes the rectangle; no menu.
    unsafe { AdjustWindowRectEx(&raw mut rect, STYLE, 0, EX_STYLE) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// `hwnd`'s client area in X pixels.
pub(super) fn client_size(hwnd: HWND) -> Size {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: writes the rectangle of a live window.
    unsafe { GetClientRect(hwnd, &raw mut rect) };
    (
        to_px(f64::from(rect.right - rect.left), 1.0),
        to_px(f64::from(rect.bottom - rect.top), 1.0),
    )
}

/// The largest client area the primary monitor's work area shows, in X
/// pixels.
pub(super) fn max_content_size() -> Option<Size> {
    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: SPI_GETWORKAREA writes one RECT.
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) } == 0 {
        return None;
    }
    let (frame_w, frame_h) = frame_size((0, 0));
    Some((
        to_px(f64::from(work.right - work.left - frame_w), 1.0),
        to_px(f64::from(work.bottom - work.top - frame_h), 1.0),
    ))
}

/// The primary display's size in millimetres and in pixels; zero when it
/// does not say.
pub(super) fn display_metrics() -> ((f64, f64), (f64, f64)) {
    // SAFETY: the screen's DC, released below.
    let dc = unsafe { GetDC(ptr::null_mut()) };
    if dc.is_null() {
        return ((0.0, 0.0), (0.0, 0.0));
    }
    // SAFETY: plain queries of a live DC.
    let caps = |index: u32| f64::from(unsafe { GetDeviceCaps(dc, index as i32) });
    let metrics = (
        (caps(HORZSIZE), caps(VERTSIZE)),
        (caps(HORZRES), caps(VERTRES)),
    );
    // SAFETY: the DC `GetDC` returned.
    unsafe { ReleaseDC(ptr::null_mut(), dc) };
    metrics
}

/// `size` X pixels in millimetres on the primary display.
pub(super) fn output_mm(size: Size, display: ((f64, f64), (f64, f64))) -> (u32, u32) {
    resize::output_mm(size, 1.0, display.0, display.1)
}

/// Title the window after the display it serves.
pub(super) fn set_title(hwnd: HWND, display: u16) {
    let title: Vec<u16> = format!("yserver DISPLAY=:{display}")
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: a live window and a NUL-terminated string.
    unsafe { SetWindowTextW(hwnd, title.as_ptr()) };
}

/// A `VK_KHR_win32_surface` surface on `hwnd`.
pub(super) fn win32_surface(hwnd: HWND) -> NativeSurface {
    // Plain integers for the core thread; the window outlives the surface
    // (the core drops every Vulkan object before it asks to quit).
    let hwnd = hwnd as vk::HWND;
    // SAFETY: the module handle of this executable.
    let instance = unsafe { GetModuleHandleW(ptr::null()) } as vk::HINSTANCE;
    NativeSurface::new(ash::khr::win32_surface::NAME, move |entry, vk_instance| {
        let loader = ash::khr::win32_surface::Instance::new(entry, vk_instance);
        let info = vk::Win32SurfaceCreateInfoKHR::default()
            .hinstance(instance)
            .hwnd(hwnd);
        // SAFETY: a live window and an instance with the extension.
        unsafe { loader.create_win32_surface(&info, None) }
    })
}
