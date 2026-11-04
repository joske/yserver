//! `--scale` on Windows: the process is per-monitor DPI aware (v2), so
//! every window coordinate is a device pixel; a logical pixel (96 DPI) is
//! the point and the monitor's DPI over 96 the backing scale, as
//! `surface::resize::PixelScale` maps them. Also the monitor's size in
//! millimetres, from its EDID. UI thread only.

use std::cell::Cell;

use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
        MonitorFromPoint, MonitorFromRect, MonitorFromWindow,
    },
    UI::{
        HiDpi::{
            AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            DPI_AWARENESS_PER_MONITOR_AWARE, GetAwarenessFromDpiAwarenessContext, GetDpiForMonitor,
            GetDpiForWindow, GetThreadDpiAwarenessContext, MDT_EFFECTIVE_DPI,
            SetProcessDpiAwarenessContext,
        },
        WindowsAndMessaging::{WINDOW_EX_STYLE, WINDOW_STYLE},
    },
};

use crate::{
    launch::ScaleOption,
    surface::{
        edid,
        resize::{BASE_DPI, PixelScale, Size},
    },
};

thread_local! {
    static OPTION: Cell<ScaleOption> = const { Cell::new(ScaleOption::Auto) };
}

/// Make the process per-monitor DPI aware (v2) unless its manifest already
/// did; before any window exists.
pub(super) fn declare_dpi_awareness() {
    // SAFETY: plain queries and a process-wide setting.
    unsafe {
        let current = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext());
        if current == DPI_AWARENESS_PER_MONITOR_AWARE {
            log::debug!("windows: per-monitor DPI aware (manifest)");
            return;
        }
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            log::warn!(
                "windows: cannot become per-monitor DPI aware: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Use `option` from now on.
pub(super) fn set_option(option: ScaleOption) {
    OPTION.set(option);
}

/// The scale at `dpi` under this run's option.
pub(super) fn at_dpi(dpi: u32) -> PixelScale {
    PixelScale::new(OPTION.get(), dpi)
}

/// `hwnd`'s DPI.
pub(super) fn window_dpi(hwnd: HWND) -> u32 {
    // SAFETY: a plain query; 0 for an invalid window.
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => BASE_DPI,
        dpi => dpi,
    }
}

/// `monitor`'s effective DPI (the display scaling).
fn monitor_dpi(monitor: HMONITOR) -> u32 {
    let (mut x, mut y) = (0u32, 0u32);
    // SAFETY: writes both values.
    let hr = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut x, &raw mut y) };
    if hr < 0 || x == 0 { BASE_DPI } else { x }
}

/// The DPI of the monitor that holds most of `rect`.
pub(super) fn dpi_at(rect: RECT) -> u32 {
    // SAFETY: a plain query.
    monitor_dpi(unsafe { MonitorFromRect(&raw const rect, MONITOR_DEFAULTTONEAREST) })
}

/// The primary monitor's DPI.
pub(super) fn primary_dpi() -> u32 {
    // SAFETY: a plain query.
    monitor_dpi(unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) })
}

/// The outer rect of a window of these styles whose client area is
/// `content`, at `dpi`.
pub(super) fn frame_rect(content: RECT, ws: WINDOW_STYLE, ex: WINDOW_EX_STYLE, dpi: u32) -> RECT {
    let mut rect = content;
    // SAFETY: writes the rectangle; no menu.
    unsafe { AdjustWindowRectExForDpi(&raw mut rect, ws, 0, ex, dpi) };
    rect
}

/// A window's scale and its monitor's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Metrics {
    pub(super) scale: PixelScale,
    pub(super) dpi: u32,
    pub(super) monitor: HMONITOR,
    /// The monitor in millimetres and in device pixels.
    display_mm: (f64, f64),
    display_px: (f64, f64),
}

/// The monitor `hwnd` is (mostly) on.
pub(super) fn monitor_of(hwnd: HWND) -> HMONITOR {
    // SAFETY: a plain query.
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
}

impl Metrics {
    /// `hwnd`'s, on the monitor it is on.
    pub(super) fn of_window(hwnd: HWND) -> Self {
        Self::at(window_dpi(hwnd), monitor_of(hwnd))
    }

    /// The primary monitor's, before a window exists.
    pub(super) fn of_primary() -> Self {
        // SAFETY: a plain query.
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        Self::at(monitor_dpi(monitor), monitor)
    }

    /// At `dpi` on the monitor holding most of `rect` (`WM_DPICHANGED`'s
    /// suggestion).
    pub(super) fn at_rect(dpi: u32, rect: RECT) -> Self {
        // SAFETY: a plain query.
        let monitor = unsafe { MonitorFromRect(&raw const rect, MONITOR_DEFAULTTONEAREST) };
        Self::at(dpi, monitor)
    }

    fn at(dpi: u32, monitor: HMONITOR) -> Self {
        let (display_mm, display_px) = physical_size(monitor, dpi);
        Self {
            scale: at_dpi(dpi),
            dpi,
            monitor,
            display_mm,
            display_px,
        }
    }

    pub(super) fn factor(&self) -> u32 {
        self.scale.factor
    }

    /// `size` X pixels in millimetres on this monitor.
    pub(super) fn mm(&self, size: Size) -> (u32, u32) {
        self.scale.mm(size, self.display_mm, self.display_px)
    }
}

/// `monitor` in millimetres and in device pixels: the millimetres from its
/// EDID, else as if `dpi` were its real resolution.
fn physical_size(monitor: HMONITOR, dpi: u32) -> ((f64, f64), (f64, f64)) {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: a monitor handle and a sized info.
    if monitor.is_null() || unsafe { GetMonitorInfoW(monitor, &raw mut info) } == 0 {
        return ((0.0, 0.0), (0.0, 0.0));
    }
    let r = info.rcMonitor;
    let px = (f64::from(r.right - r.left), f64::from(r.bottom - r.top));
    let edid = super::edid::of_monitor(monitor);
    let mm = edid.as_deref().and_then(edid::size_mm);
    log::debug!(
        "windows: monitor {}x{} px at {dpi} DPI, EDID {} bytes, size {mm:?} mm",
        px.0,
        px.1,
        edid.as_ref().map_or(0, Vec::len)
    );
    let mm = mm.map_or_else(
        || {
            let per_px = 25.4 / f64::from(dpi.max(1));
            (px.0 * per_px, px.1 * per_px)
        },
        |(w, h)| {
            // EDID gives the panel's native orientation; a rotated
            // monitor has its sides the other way round.
            let (w, h) = (f64::from(w), f64::from(h));
            if (w > h) == (px.0 > px.1) {
                (w, h)
            } else {
                (h, w)
            }
        },
    );
    (mm, px)
}
