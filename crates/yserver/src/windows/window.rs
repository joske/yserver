//! The window: a resizable top-level whose client area is the X screen,
//! `factor`x`factor` device pixels per X pixel, and the Vulkan surface on
//! it.

use std::{io, ptr};

use ash::vk;
use windows_sys::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::{GetMonitorInfoW, MONITORINFO},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, GetClientRect, IDC_ARROW,
            LoadCursorW, RegisterClassExW, SetWindowTextW, WINDOW_EX_STYLE, WNDCLASSEXW, WNDPROC,
            WS_OVERLAPPEDWINDOW,
        },
    },
    core::w,
};

use super::scale::{self, Metrics};
use crate::surface::{resize::Size, swapchain::NativeSurface};

const CLASS_NAME: windows_sys::core::PCWSTR = w!("yserver");
const STYLE: u32 = WS_OVERLAPPEDWINDOW;
const EX_STYLE: WINDOW_EX_STYLE = 0;

/// A titled, closable, minimisable, resizable window whose client area is
/// `size` X pixels at `metrics` (the primary monitor's), not yet shown;
/// `wndproc` handles its messages.
///
/// # Errors
///
/// The class or the window could not be created.
pub(super) fn create(size: Size, metrics: &Metrics, wndproc: WNDPROC) -> io::Result<HWND> {
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
    let (width, height) = frame_size(metrics.scale.to_device(size), metrics.dpi);
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

/// The outer size at `dpi` of a window whose client area is `device`
/// pixels.
pub(super) fn frame_size(device: (i32, i32), dpi: u32) -> (i32, i32) {
    let content = RECT {
        left: 0,
        top: 0,
        right: device.0,
        bottom: device.1,
    };
    let rect = scale::frame_rect(content, STYLE, EX_STYLE, dpi);
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// `hwnd`'s client area in device pixels.
pub(super) fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: writes the rectangle of a live window.
    unsafe { GetClientRect(hwnd, &raw mut rect) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// The largest client area the work area of `metrics`' monitor shows, in
/// X pixels.
pub(super) fn max_content_size(metrics: &Metrics) -> Option<Size> {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: a monitor handle and a sized info.
    if unsafe { GetMonitorInfoW(metrics.monitor, &raw mut info) } == 0 {
        return None;
    }
    let work = info.rcWork;
    let (frame_w, frame_h) = frame_size((0, 0), metrics.dpi);
    Some(metrics.scale.to_x((
        work.right - work.left - frame_w,
        work.bottom - work.top - frame_h,
    )))
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
