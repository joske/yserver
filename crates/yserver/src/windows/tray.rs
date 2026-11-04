//! The notification-area icon, in both modes: its menu opens the Settings
//! and Exit stops the server (as XWin's tray icon does; rootless has no root
//! window to close), asking first while X clients are connected. Its tip
//! names the display. UI thread only.

use std::{
    cell::RefCell,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT},
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, MF_STRING, PostMessageW,
                RegisterWindowMessageW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_NULL, WM_RBUTTONUP,
            },
        },
    },
    core::w,
};

use crate::surface::link::ShellLink;

/// The icon's callback message (the rooted window's own are below it).
const WM_TRAY: u32 = WM_APP + 3;

/// The menu's items.
const SETTINGS: usize = 1;
const EXIT: usize = 2;

struct Tray {
    hwnd: HWND,
    link: Arc<ShellLink>,
    /// Established X clients, kept by the core.
    clients: Arc<AtomicUsize>,
    display: Option<u16>,
    /// The message Explorer broadcasts when the taskbar is (re)created.
    taskbar_created: u32,
    /// The user already confirmed quitting.
    quitting: bool,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

impl Tray {
    fn data(&self) -> NOTIFYICONDATAW {
        // SAFETY: zeroed is a valid NOTIFYICONDATAW; the fields used are set.
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0);
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = super::icon::small();
        let tip = match self.display {
            Some(display) => format!("yserver DISPLAY=:{display}"),
            None => "yserver".to_owned(),
        };
        for (to, from) in data.szTip.iter_mut().zip(tip.encode_utf16()) {
            *to = from;
        }
        data
    }

    fn notify(&self, message: u32) {
        let data = self.data();
        // SAFETY: a fully initialised description of our icon.
        unsafe { Shell_NotifyIconW(message, &raw const data) };
    }
}

/// Show the icon, owned by `hwnd` (the rooted window or the rootless
/// message window).
pub(super) fn install(hwnd: HWND, link: Arc<ShellLink>, clients: Arc<AtomicUsize>) {
    // SAFETY: a NUL-terminated string.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    let tray = Tray {
        hwnd,
        link,
        clients,
        display: None,
        taskbar_created,
        quitting: false,
    };
    tray.notify(NIM_ADD);
    TRAY.set(Some(tray));
}

/// Remove the icon.
pub(super) fn remove() {
    if let Some(tray) = TRAY.take() {
        tray.notify(NIM_DELETE);
    }
}

/// The server listens on `display`.
pub(super) fn set_display(display: u16) {
    TRAY.with_borrow_mut(|tray| {
        if let Some(tray) = tray {
            tray.display = Some(display);
            tray.notify(NIM_MODIFY);
        }
    });
}

/// A message to the message window: `true` when it was the icon's.
pub(super) fn message(message: u32, lparam: LPARAM) -> bool {
    let Some((hwnd, taskbar_created)) =
        TRAY.with_borrow(|tray| tray.as_ref().map(|t| (t.hwnd, t.taskbar_created)))
    else {
        return false;
    };
    if message == taskbar_created {
        // Explorer restarted: the icon is gone.
        TRAY.with_borrow(|tray| {
            if let Some(tray) = tray {
                tray.notify(NIM_ADD);
            }
        });
        return true;
    }
    if message != WM_TRAY {
        return false;
    }
    let event = u32::try_from(lparam & 0xFFFF).unwrap_or(0);
    if matches!(event, WM_RBUTTONUP | WM_CONTEXTMENU) {
        match menu(hwnd) {
            SETTINGS => super::settings_dialog::show(),
            EXIT => exit(hwnd),
            _ => {}
        }
    }
    true
}

/// Show the menu at the pointer; the chosen item, 0 for none.
fn menu(hwnd: HWND) -> usize {
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: a fresh menu, destroyed below; the window is ours. The menu
    // closes on a click elsewhere only while its owner is the foreground
    // window, and the trailing `WM_NULL` lets it go (KB135788).
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return 0;
        }
        AppendMenuW(menu, MF_STRING, SETTINGS, w!("Settings…"));
        AppendMenuW(menu, MF_STRING, EXIT, w!("Exit yserver"));
        GetCursorPos(&raw mut at);
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            at.x,
            at.y,
            0,
            hwnd,
            ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        usize::try_from(chosen).unwrap_or(0)
    }
}

/// Exit: ask while X clients are connected, then shut down.
fn exit(hwnd: HWND) {
    let Some((clients, quitting)) = TRAY.with_borrow(|tray| {
        tray.as_ref()
            .map(|t| (t.clients.load(Ordering::Acquire), t.quitting))
    }) else {
        return;
    };
    // The box's modal loop runs with no borrow held.
    if !quitting && !super::app::confirm_quit(hwnd, clients, false) {
        return;
    }
    TRAY.with_borrow_mut(|tray| {
        if let Some(tray) = tray {
            tray.quitting = true;
            tray.link.request_shutdown();
        }
    });
}
