//! The window procedures: a top-level's input goes straight to the core
//! through `ShellInput`; everything that needs the shell is queued
//! ([`shell::on_ui`]), as the shell's own Win32 calls send these messages.

use std::ptr;

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    Graphics::Gdi::{ClientToScreen, ValidateRect},
    UI::{
        Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        WindowsAndMessaging::{
            CREATESTRUCTW, DefWindowProcW, GetMessageTime, HTCLIENT, HTTRANSPARENT, MA_NOACTIVATE,
            MINMAXINFO, PostQuitMessage, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, WA_INACTIVE,
            WM_ACTIVATE, WM_ACTIVATEAPP, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND,
            WM_GETDPISCALEDSIZE, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEHWHEEL,
            WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT,
            WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZING, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_WINDOWPOSCHANGED, WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    },
};
use yserver_core::rootless::ToplevelId;

use super::{
    shell::{self, WM_RUN},
    window,
};
use crate::windows::{
    input::{self, RawKeyMessage, ShellInput},
    tray,
};

/// `WM_MOUSELEAVE` (`winuser.h`), from the `Win32_UI_Controls` feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// The message window's procedure: the shell's queued work, the tray
/// icon, the end.
pub(super) unsafe extern "system" fn message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if tray::message(message, lparam) {
        return 0;
    }
    match message {
        WM_RUN => shell::run_jobs(),
        WM_DESTROY => {
            tray::remove();
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
        // SAFETY: the default handling of a message for this window.
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
    0
}

/// A top-level's window procedure.
pub(super) unsafe extern "system" fn toplevel(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling of a message for this window.
    let default = || unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    match message {
        WM_NCCREATE => {
            // SAFETY: `lparam` is the CREATESTRUCTW of this window, whose
            // creation data is the `WindowData` box (`window::create`).
            let data = unsafe { (*(lparam as *const CREATESTRUCTW)).lpCreateParams };
            window::attach(hwnd, data.cast());
            return default();
        }
        WM_NCDESTROY => {
            window::detach(hwnd);
            return default();
        }
        _ => {}
    }
    let Some(data) = window::data(hwnd) else {
        return default();
    };
    let id = data.id;
    match message {
        WM_GETMINMAXINFO => {
            // SAFETY: `lparam` is the system's MINMAXINFO for this message.
            let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            if let Some((width, height)) = data.min.get() {
                info.ptMinTrackSize.x = width;
                info.ptMinTrackSize.y = height;
            }
            if let Some((width, height)) = data.max.get() {
                info.ptMaxTrackSize.x = width;
                info.ptMaxTrackSize.y = height;
            }
            return 0;
        }
        WM_SIZING => {
            if let Some(increments) = data.increments.get() {
                // SAFETY: `lparam` is the dragged outer rect of this window.
                let frame = unsafe { &mut *(lparam as *mut RECT) };
                window::snap(hwnd, frame, wparam as u32, increments);
                return 1;
            }
            return default();
        }
        // One scale for every monitor: the client area keeps its device
        // pixels, so X keeps the geometry; only the frame takes the new DPI.
        WM_GETDPISCALEDSIZE => {
            let dpi = u32::try_from(wparam).unwrap_or(crate::surface::resize::BASE_DPI);
            // SAFETY: `lparam` is the outer size to fill for this message.
            let size = unsafe { &mut *(lparam as *mut SIZE) };
            let (ws, ex) = window::styles(hwnd);
            let content = window::content_rect(hwnd);
            let client = RECT {
                left: 0,
                top: 0,
                right: content.right - content.left,
                bottom: content.bottom - content.top,
            };
            let frame = window::frame_rect(client, ws, ex, dpi);
            size.cx = frame.right - frame.left;
            size.cy = frame.bottom - frame.top;
            return 1;
        }
        WM_DPICHANGED => {
            // SAFETY: `lparam` is the suggested outer rect for this message.
            let r = unsafe { *(lparam as *const RECT) };
            log::debug!(
                "windows: {id:?} at DPI {}: {},{} {}x{}",
                wparam & 0xFFFF,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top
            );
            // SAFETY: our window; `WM_WINDOWPOSCHANGED` only queues work.
            unsafe {
                SetWindowPos(
                    hwnd,
                    ptr::null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            // The outer limits are at the old DPI's frame.
            shell::on_ui(move |shell| shell.dpi_changed(id));
        }
        // Not under the pointer until its first frame shows.
        WM_NCHITTEST if !data.shown.get() => return HTTRANSPARENT as LRESULT,
        // A popup takes the click without taking the focus (§6.2).
        WM_MOUSEACTIVATE if data.popup => return MA_NOACTIVATE as LRESULT,
        // Vulkan paints the client area.
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            // SAFETY: a window of this thread.
            unsafe { ValidateRect(hwnd, ptr::null()) };
        }
        WM_CLOSE => match shell::now(|shell| shell.close_clicked(id)) {
            Some(Some(ask)) => ask.run(),
            Some(None) => {}
            // Busy now: the kill question waits for the next click.
            None => shell::on_ui(move |shell| {
                let _ = shell.close_clicked(id);
            }),
        },
        WM_WINDOWPOSCHANGED => {
            shell::on_ui(move |shell| shell.frame_changed(id));
            return default();
        }
        WM_ACTIVATE => {
            if u32::try_from(wparam & 0xFFFF).ok() == Some(WA_INACTIVE) {
                shell::on_ui(shell::RootlessShell::resigned_key);
            } else {
                shell::on_ui(move |shell| shell.became_key(id));
            }
            return default();
        }
        WM_ACTIVATEAPP => {
            if wparam == 0 {
                shell::on_ui(shell::RootlessShell::app_deactivated);
            } else {
                shell::on_ui(shell::RootlessShell::sync_stacking);
            }
            return default();
        }
        WM_SETCURSOR if u32::try_from(lparam & 0xFFFF).ok() == Some(HTCLIENT) => {
            crate::windows::cursor::apply_current();
            return 1;
        }
        _ => return input_message(hwnd, id, message, wparam, lparam).unwrap_or_else(default),
    }
    0
}

/// Keyboard and mouse messages; `None` for any other.
fn input_message(
    hwnd: HWND,
    id: ToplevelId,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if !matches!(
        message,
        WM_KEYDOWN
            | WM_SYSKEYDOWN
            | WM_KEYUP
            | WM_SYSKEYUP
            | WM_MOUSEMOVE
            | WM_MOUSELEAVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    ) {
        return None;
    }
    let input = shell::input()?;
    match message {
        // Not to `DefWindowProcW`: Alt and F10 would open the window menu,
        // Alt+F4 would close; X gets them all (D6).
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            // SAFETY: a plain query of the message being handled.
            let time = unsafe { GetMessageTime() } as u32;
            input.key(hwnd, RawKeyMessage::new(wparam, lparam, pressed, time));
        }
        WM_MOUSEMOVE => {
            track_leave(hwnd);
            match drag_target(&input, hwnd, lparam) {
                Some((under, local)) => input.dragged_local(under, local),
                None => input.motion_local(id, local(&input, lparam)),
            }
        }
        WM_MOUSELEAVE => {
            if let Some(data) = window::data(hwnd) {
                data.tracking.set(false);
            }
            input.exited_local(id);
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            if let Some(number) = input::x_button(wparam) {
                button(
                    &input,
                    hwnd,
                    id,
                    number,
                    message == WM_XBUTTONDOWN,
                    wparam,
                    lparam,
                );
            }
            return Some(1);
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16;
            input.wheel(delta, message == WM_MOUSEHWHEEL);
        }
        _ => {
            let (number, pressed) = match message {
                WM_LBUTTONDOWN => (0, true),
                WM_LBUTTONUP => (0, false),
                WM_RBUTTONDOWN => (1, true),
                WM_RBUTTONUP => (1, false),
                WM_MBUTTONDOWN => (2, true),
                _ => (2, false),
            };
            button(&input, hwnd, id, number, pressed, wparam, lparam);
        }
    }
    Some(0)
}

fn button(
    input: &ShellInput,
    hwnd: HWND,
    id: ToplevelId,
    number: i64,
    pressed: bool,
    wparam: WPARAM,
    lparam: LPARAM,
) {
    // A release ends a drag where the pointer is, not where it began.
    let under = if pressed {
        None
    } else {
        drag_target(input, hwnd, lparam)
    };
    let at = local(input, lparam);
    input.button_local(hwnd, id, number, pressed, at, wparam, under);
}

/// The X pixel of a mouse message's client position.
fn local(input: &ShellInput, lparam: LPARAM) -> (f64, f64) {
    input.to_x(input::client_position(lparam))
}

/// Ask for `WM_MOUSELEAVE` once per stay in `hwnd`.
fn track_leave(hwnd: HWND) {
    let Some(data) = window::data(hwnd) else {
        return;
    };
    if data.tracking.replace(true) {
        return;
    }
    let mut track = TRACKMOUSEEVENT {
        cbSize: u32::try_from(size_of::<TRACKMOUSEEVENT>()).unwrap_or(0),
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: a live window of this thread.
    unsafe { TrackMouseEvent(&raw mut track) };
}

/// While a button is held, Windows sends every mouse message to the window
/// that got the press (the capture): the top-level really under the
/// message's position, and the position in it (or in the current one, over
/// none of ours).
fn drag_target(
    input: &ShellInput,
    hwnd: HWND,
    lparam: LPARAM,
) -> Option<(Option<ToplevelId>, (f64, f64))> {
    let (dragging, current) = input.drag_state();
    if !dragging {
        return None;
    }
    let (x, y) = input::client_position(lparam);
    let mut at = POINT { x, y };
    // SAFETY: a live window; writes `at`.
    unsafe { ClientToScreen(hwnd, &raw mut at) };
    let under = window::toplevel_at(at);
    let target = window::hwnd_of(under.or(current)?)?;
    let content = window::content_rect(target);
    Some((under, input.to_x((at.x - content.left, at.y - content.top))))
}
