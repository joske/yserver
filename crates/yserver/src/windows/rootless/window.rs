//! A top-level's Win32 window from its `NativeStyle`, the per-window data
//! its window procedure reads without the shell, and the display space in
//! the y-up convention of `GlobalSpace`.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io, ptr,
};

use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, TRUE},
        Graphics::{
            Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute},
            Gdi::{
                ClientToScreen, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
                MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
            },
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CW_USEDEFAULT, CreateWindowExW, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GWLP_USERDATA,
            GetAncestor, GetClientRect, GetWindowLongPtrW, MONITORINFOF_PRIMARY, RegisterClassExW,
            SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_STYLE, WMSZ_BOTTOMLEFT, WMSZ_LEFT, WMSZ_TOP,
            WMSZ_TOPLEFT, WMSZ_TOPRIGHT, WNDCLASSEXW, WNDPROC, WS_CAPTION, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED,
            WS_POPUP, WS_SYSMENU, WS_THICKFRAME, WindowFromPoint,
        },
    },
    core::w,
};
use yserver_core::rootless::ToplevelId;

use crate::{
    rootless::native::{
        space::NativeRect,
        style::{NativeLevel, NativeStyle},
    },
    windows::scale,
};

const TOPLEVEL_CLASS: windows_sys::core::PCWSTR = w!("yserver-toplevel");
const MESSAGE_CLASS: windows_sys::core::PCWSTR = w!("yserver-rootless");

/// What the window procedure reads of a top-level without the shell; owned
/// by the window (`GWLP_USERDATA`), freed at `WM_NCDESTROY`.
pub(super) struct WindowData {
    pub(super) id: ToplevelId,
    pub(super) popup: bool,
    /// Uncloaked: its first frame is on screen.
    pub(super) shown: Cell<bool>,
    /// `WM_MOUSELEAVE` is asked for.
    pub(super) tracking: Cell<bool>,
    /// The outer size limits `WM_GETMINMAXINFO` answers.
    pub(super) min: Cell<Option<(i32, i32)>>,
    pub(super) max: Cell<Option<(i32, i32)>>,
    /// The client size `WM_SIZING` snaps to: base + n × increment.
    pub(super) increments: Cell<Option<Increments>>,
}

/// `WM_NORMAL_HINTS` resize increments (at least the copy factor), in
/// client device pixels.
#[derive(Clone, Copy, Debug)]
pub(super) struct Increments {
    pub(super) base: (i32, i32),
    pub(super) inc: (i32, i32),
}

/// `WM_SIZING`: snap the outer rect `frame` of `hwnd`, dragged at `edge`
/// (`WMSZ_*`), to `increments`; the dragged sides move.
pub(super) fn snap(hwnd: HWND, frame: &mut RECT, edge: u32, increments: Increments) {
    let (ws, ex) = styles(hwnd);
    let extra = frame_rect(
        RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        ws,
        ex,
        scale::window_dpi(hwnd),
    );
    let snap = |outer: i32, extra: i32, base: i32, inc: i32| {
        let content = outer - extra;
        if inc <= 1 || content <= base {
            return outer;
        }
        base + (content - base) / inc * inc + extra
    };
    let width = snap(
        frame.right - frame.left,
        extra.right - extra.left,
        increments.base.0,
        increments.inc.0,
    );
    let height = snap(
        frame.bottom - frame.top,
        extra.bottom - extra.top,
        increments.base.1,
        increments.inc.1,
    );
    if matches!(edge, WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT) {
        frame.left = frame.right - width;
    } else {
        frame.right = frame.left + width;
    }
    if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
        frame.top = frame.bottom - height;
    } else {
        frame.bottom = frame.top + height;
    }
}

thread_local! {
    /// Every live top-level's window, by id.
    static HWNDS: RefCell<HashMap<ToplevelId, HWND>> = RefCell::new(HashMap::new());
}

/// The data of `hwnd`, a window of our top-level class (its window
/// procedure's own).
pub(super) fn data<'a>(hwnd: HWND) -> Option<&'a WindowData> {
    // SAFETY: a plain query; the pointer is ours and lives until
    // `WM_NCDESTROY` (`attach`, `detach`).
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowData;
    // SAFETY: as above; only top-levels of our class set it.
    unsafe { ptr.as_ref() }
}

/// `WM_NCCREATE`: own the data passed to `CreateWindowExW`.
pub(super) fn attach(hwnd: HWND, data: *mut WindowData) {
    // SAFETY: the box `create` leaked; read before the window is shown.
    let id = unsafe { (*data).id };
    HWNDS.with_borrow_mut(|hwnds| hwnds.insert(id, hwnd));
    // SAFETY: our window, during its creation.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, data as isize) };
}

/// `WM_NCDESTROY`: free the data.
pub(super) fn detach(hwnd: HWND) {
    // SAFETY: our window; the data is not read after this.
    let ptr = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut WindowData;
    if ptr.is_null() {
        return;
    }
    // SAFETY: the box `create` leaked, released once.
    let data = unsafe { Box::from_raw(ptr) };
    HWNDS.with_borrow_mut(|hwnds| {
        if hwnds.get(&data.id) == Some(&hwnd) {
            hwnds.remove(&data.id);
        }
    });
}

/// The window of top-level `id`.
pub(super) fn hwnd_of(id: ToplevelId) -> Option<HWND> {
    HWNDS.with_borrow(|hwnds| hwnds.get(&id).copied())
}

/// Register both window classes.
///
/// # Errors
///
/// A class could not be registered.
pub(super) fn register(toplevel: WNDPROC, message: WNDPROC) -> io::Result<()> {
    // SAFETY: the module handle of this executable.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    for (name, wndproc) in [(TOPLEVEL_CLASS, toplevel), (MESSAGE_CLASS, message)] {
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
            style: 0,
            lpfnWndProc: wndproc,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            // The client's `_NET_WM_ICON` could show here; yserver's for now.
            hIcon: crate::windows::icon::large(),
            // None: `WM_SETCURSOR` shows the X cursor over the client area.
            hCursor: ptr::null_mut(),
            // No background brush: Vulkan paints every pixel.
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: name,
            hIconSm: crate::windows::icon::small(),
        };
        // SAFETY: a fully initialised class description.
        if unsafe { RegisterClassExW(&raw const class) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The window the core's commands are posted to and the tray icon talks
/// to: a top-level never shown, as the tray menu needs a window that can be
/// the foreground one (a message-only window cannot).
///
/// # Errors
///
/// The window could not be created.
pub(super) fn create_message_window() -> io::Result<HWND> {
    // SAFETY: a registered class; no parent.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            MESSAGE_CLASS,
            w!("yserver"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(hwnd)
}

/// The window style of `style`: native title bar and frame for a titled
/// window, none for the rest.
pub(super) fn window_style(style: &NativeStyle) -> (WINDOW_STYLE, WINDOW_EX_STYLE) {
    if style.level == NativeLevel::PopUp {
        return (
            WS_POPUP,
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        );
    }
    let mut ws = if style.titled {
        WS_OVERLAPPED | WS_CAPTION
    } else {
        WS_POPUP
    };
    if style.closable {
        ws |= WS_SYSMENU;
    }
    if style.miniaturizable {
        ws |= WS_MINIMIZEBOX;
    }
    if style.titled && style.resizable {
        ws |= WS_THICKFRAME | WS_MAXIMIZEBOX;
    }
    let ex = if style.level == NativeLevel::Floating {
        WS_EX_TOPMOST
    } else {
        0
    };
    (ws, ex)
}

/// A cloaked window of `style` whose client area is `content` (screen
/// device pixels), owned by `owner`; at the system's default position when
/// `place`. Shown but cloaked: a present to it acquires, nothing is seen
/// until [`uncloak`].
///
/// # Errors
///
/// The window could not be created.
pub(super) fn create(
    style: &NativeStyle,
    content: RECT,
    title: &str,
    data: WindowData,
    owner: Option<HWND>,
    place: bool,
) -> io::Result<HWND> {
    let (ws, ex) = window_style(style);
    let frame = frame_rect(content, ws, ex, scale::dpi_at(content));
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let data = Box::into_raw(Box::new(data));
    let (x, y) = if place {
        (CW_USEDEFAULT, CW_USEDEFAULT)
    } else {
        (frame.left, frame.top)
    };
    // SAFETY: a registered class; `WM_NCCREATE` takes the data, which
    // `WM_NCDESTROY` frees.
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            TOPLEVEL_CLASS,
            title.as_ptr(),
            ws,
            x,
            y,
            frame.right - frame.left,
            frame.bottom - frame.top,
            owner.unwrap_or(ptr::null_mut()),
            ptr::null_mut(),
            GetModuleHandleW(ptr::null()),
            data.cast_const().cast(),
        )
    };
    if hwnd.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: no window took the box.
        drop(unsafe { Box::from_raw(data) });
        return Err(error);
    }
    set_cloaked(hwnd, true);
    Ok(hwnd)
}

/// Show or hide `hwnd`'s content without unmapping it (DWM cloaking).
pub(super) fn set_cloaked(hwnd: HWND, cloaked: bool) {
    let value = i32::from(cloaked);
    // SAFETY: our window; a BOOL-sized attribute.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK as u32,
            (&raw const value).cast(),
            size_of::<i32>() as u32,
        );
    }
}

/// `hwnd`'s style and extended style.
pub(super) fn styles(hwnd: HWND) -> (WINDOW_STYLE, WINDOW_EX_STYLE) {
    // SAFETY: plain queries of a live window.
    unsafe {
        (
            GetWindowLongPtrW(hwnd, GWL_STYLE) as WINDOW_STYLE,
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as WINDOW_EX_STYLE,
        )
    }
}

pub(super) use crate::windows::scale::frame_rect;

/// `hwnd`'s client area in screen device pixels.
pub(super) fn content_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: a live window; writes the rectangle and the point.
    unsafe {
        GetClientRect(hwnd, &raw mut rect);
        ClientToScreen(hwnd, &raw mut origin);
    }
    RECT {
        left: origin.x,
        top: origin.y,
        right: origin.x + rect.right,
        bottom: origin.y + rect.bottom,
    }
}

/// Our top-level whose client area holds the screen point `at`, if any.
pub(super) fn toplevel_at(at: POINT) -> Option<ToplevelId> {
    // SAFETY: plain queries.
    let hwnd = unsafe { GetAncestor(WindowFromPoint(at), GA_ROOT) };
    // Another program's window keeps its own data there.
    if !HWNDS.with_borrow(|hwnds| hwnds.values().any(|&h| h == hwnd)) {
        return None;
    }
    let data = data(hwnd)?;
    let content = content_rect(hwnd);
    ((content.left..content.right).contains(&at.x) && (content.top..content.bottom).contains(&at.y))
        .then_some(data.id)
}

/// A screen rect in `GlobalSpace`'s convention: y up, so the screen's
/// y-down rows mirror.
pub(super) fn native(rect: RECT) -> NativeRect {
    NativeRect {
        x: f64::from(rect.left),
        y: -f64::from(rect.bottom),
        width: f64::from(rect.right - rect.left),
        height: f64::from(rect.bottom - rect.top),
    }
}

/// The inverse of [`native`], whole pixels.
pub(super) fn screen(rect: NativeRect) -> RECT {
    #[allow(clippy::cast_possible_truncation)]
    let px = |v: f64| v.round() as i32;
    RECT {
        left: px(rect.x),
        top: px(-(rect.y + rect.height)),
        right: px(rect.x + rect.width),
        bottom: px(-rect.y),
    }
}

/// Every monitor's rect, the primary first.
pub(super) fn monitors() -> Vec<RECT> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        list: LPARAM,
    ) -> windows_sys::core::BOOL {
        // SAFETY: zeroed is a valid MONITORINFO; `cbSize` is set.
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0);
        // SAFETY: the monitor being enumerated; `list` is the vector below.
        unsafe {
            if GetMonitorInfoW(monitor, &raw mut info) != 0 {
                let list = &mut *(list as *mut Vec<(bool, RECT)>);
                list.push((info.dwFlags & MONITORINFOF_PRIMARY != 0, info.rcMonitor));
            }
        }
        TRUE
    }
    let mut list: Vec<(bool, RECT)> = Vec::new();
    // SAFETY: the whole desktop; the callback only runs inside this call.
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(collect),
            (&raw mut list) as LPARAM,
        );
    }
    list.sort_by_key(|(primary, _)| !primary);
    list.into_iter().map(|(_, rect)| rect).collect()
}

/// The monitor holding `at` (else the primary): its rect and its work
/// area (without the taskbar).
pub(super) fn monitor_at(at: POINT) -> Option<(RECT, RECT)> {
    // SAFETY: a plain query.
    let monitor = unsafe { MonitorFromPoint(at, MONITOR_DEFAULTTOPRIMARY) };
    // SAFETY: zeroed is a valid MONITORINFO; `cbSize` is set.
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0);
    // SAFETY: a monitor handle from above.
    (unsafe { GetMonitorInfoW(monitor, &raw mut info) } != 0)
        .then_some((info.rcMonitor, info.rcWork))
}
