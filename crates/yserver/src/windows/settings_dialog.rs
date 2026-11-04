//! The Settings dialog (the tray icon's Settings…, and the rooted window's
//! system menu), built in code: one instance, brought to front on reopen.
//! As on macOS, every change is written to the settings file at once; the
//! live settings also apply at once, mode and scale at the next start. A
//! setting a command-line flag fixed for this run is shown disabled and
//! never written. Start at sign-in is the `Run` registry value, never
//! stored; Restart quits as Exit does and starts yserver again with the
//! same arguments (`super::run`). UI thread only.

use std::{
    cell::RefCell,
    ffi::c_void,
    path::PathBuf,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GlobalFree, HWND, LPARAM, LRESULT, RECT, SIZE,
            WPARAM,
        },
        Graphics::Gdi::{
            COLOR_BTNFACE, COLOR_GRAYTEXT, CreateFontIndirectW, DeleteObject, GetDC, GetSysColor,
            GetSysColorBrush, GetTextExtentPoint32W, HDC, HFONT, ReleaseDC, SelectObject,
            SetBkMode, SetTextColor, TRANSPARENT,
        },
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            LibraryLoader::GetModuleHandleW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Registry::{
                HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
                RegSetKeyValueW,
            },
        },
        UI::{
            Input::KeyboardAndMouse::EnableWindow,
            WindowsAndMessaging::{
                AdjustWindowRectEx, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX,
                BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL,
                CB_SETCURSEL, CBN_SELCHANGE, CBS_DROPDOWNLIST, CW_USEDEFAULT, CreateWindowExW,
                DefWindowProcW, DestroyWindow, GetWindowRect, IDC_ARROW, IDCANCEL, IDI_APPLICATION,
                IsDialogMessageW, LoadCursorW, LoadIconW, MB_ICONWARNING, MB_OK, MSG, MessageBoxW,
                NONCLIENTMETRICSW, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SW_SHOW,
                SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW,
                ShowWindow, SystemParametersInfoW, WA_INACTIVE, WINDOW_STYLE, WM_ACTIVATE,
                WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY, WM_SETFONT, WNDCLASSEXW,
                WS_CAPTION, WS_CHILD, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_GROUP,
                WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
            },
        },
    },
    core::{PCWSTR, w},
};

use crate::{
    launch::ScaleOption,
    settings::{
        KEY_MODE, KEY_SCALE, KEY_SYNC_CLIPBOARD, LoginItem, Mode, SetByArgv, Store,
        file::FileStore, login, restart_view, scale_str,
    },
    surface::link::ShellLink,
};

/// The scale list's items, in order.
const SCALES: [(ScaleOption, &str); 3] = [
    (ScaleOption::Auto, "Automatic"),
    (ScaleOption::Factor(1), "1 (one X pixel per pixel at 100%)"),
    (
        ScaleOption::Factor(2),
        "2 (one X pixel per screen pixel at 200%)",
    ),
];

/// The `Run` key and yserver's value in it.
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("yserver");

const CLASS_NAME: PCWSTR = w!("yserver settings");

/// The controls' ids.
const ID_COPY: u16 = 101;
const ID_ROOTLESS: u16 = 102;
const ID_WINDOW: u16 = 103;
const ID_SCALE: u16 = 104;
const ID_RESTART: u16 = 105;
const ID_CLIPBOARD: u16 = 106;
const ID_LOGIN: u16 = 107;

/// `BST_CHECKED`, in `Win32_UI_Controls`.
const CHECKED: usize = 1;
/// `CF_UNICODETEXT`, in `Win32_System_Ole`.
const CF_UNICODETEXT: u32 = 13;

/// The settings file: `%APPDATA%\yserver\settings.conf`.
pub(super) fn store() -> FileStore {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    if appdata.is_none() {
        log::warn!("windows: no APPDATA; the settings file is in the working directory");
    }
    FileStore::open(
        appdata
            .unwrap_or_default()
            .join("yserver")
            .join("settings.conf"),
    )
}

/// What the dialog shows and changes besides the stored settings.
pub(super) struct Context {
    pub(super) link: Arc<ShellLink>,
    /// Established X clients, kept by the core.
    pub(super) clients: Arc<AtomicUsize>,
    /// The clipboard bridge's switch.
    pub(super) sync_clipboard: Arc<AtomicBool>,
    pub(super) set_by_argv: SetByArgv,
    /// This run's mode and scale, shown for what argv fixed.
    pub(super) mode: Mode,
    pub(super) scale: ScaleOption,
    /// The rooted window, which the dialog stays above; none when rootless.
    pub(super) owner: HWND,
}

/// The open dialog.
struct Dialog {
    hwnd: HWND,
    font: HFONT,
    display: HWND,
    login: HWND,
    /// The secondary labels, drawn grey.
    notes: Vec<HWND>,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
    static DISPLAY: RefCell<Option<u16>> = const { RefCell::new(None) };
    static DIALOG: RefCell<Option<Dialog>> = const { RefCell::new(None) };
}

/// Make the dialog available on this thread.
pub(super) fn install(context: Context) {
    CONTEXT.set(Some(context));
}

/// Close the dialog and forget the context.
pub(super) fn uninstall() {
    close();
    CONTEXT.set(None);
}

/// The server listens on `display`.
pub(super) fn set_display(display: u16) {
    DISPLAY.set(Some(display));
    DIALOG.with_borrow(|dialog| {
        if let Some(dialog) = dialog {
            set_text(dialog.display, &display_text());
        }
    });
}

/// The display as clients name it, once known.
fn display_name() -> Option<String> {
    DISPLAY.with_borrow(|display| display.map(|n| format!("localhost:{n}")))
}

fn display_text() -> String {
    display_name().unwrap_or_else(|| "starting".to_owned())
}

/// Keyboard navigation for the dialog: `true` when `msg` was its.
pub(super) fn is_dialog_message(msg: &MSG) -> bool {
    let Some(hwnd) = DIALOG.with_borrow(|dialog| dialog.as_ref().map(|d| d.hwnd)) else {
        return false;
    };
    // SAFETY: a live window of this thread and a message from its queue.
    unsafe { IsDialogMessageW(hwnd, msg) != 0 }
}

/// Open the dialog, or bring it to front.
pub(super) fn show() {
    if let Some(hwnd) = DIALOG.with_borrow(|dialog| dialog.as_ref().map(|d| d.hwnd)) {
        // SAFETY: a live window of this thread.
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
        return;
    }
    if let Err(e) = create() {
        log::error!("windows: cannot open the settings: {e}");
    }
}

fn close() {
    let hwnd = DIALOG.with_borrow(|dialog| dialog.as_ref().map(|d| d.hwnd));
    if let Some(hwnd) = hwnd {
        // SAFETY: a live window of this thread; `WM_DESTROY` forgets it.
        unsafe { DestroyWindow(hwnd) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn set_text(hwnd: HWND, text: &str) {
    let text = wide(text);
    // SAFETY: a live window and a NUL-terminated string.
    unsafe { SetWindowTextW(hwnd, text.as_ptr()) };
}

fn checked(hwnd: HWND) -> bool {
    // SAFETY: a button of this thread.
    unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == CHECKED as LRESULT }
}

fn set_checked(hwnd: HWND, on: bool) {
    // SAFETY: a button of this thread.
    unsafe { SendMessageW(hwnd, BM_SETCHECK, usize::from(on), 0) };
}

/// The command line that started this run.
fn this_command_line() -> std::io::Result<String> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    Ok(login::command_line(&exe, &args))
}

/// Whether the `Run` value is there.
fn login_item() -> LoginItem {
    let mut size = 0u32;
    // SAFETY: a size query, no data buffer.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut size,
        )
    };
    if status == ERROR_SUCCESS {
        LoginItem::Enabled
    } else {
        LoginItem::NotRegistered
    }
}

/// Write or delete the `Run` value; the error's text on failure.
fn set_login_item(on: bool) -> Result<(), String> {
    let status = if on {
        let line = wide(&this_command_line().map_err(|e| e.to_string())?);
        let bytes = u32::try_from(line.len() * 2).map_err(|e| e.to_string())?;
        // SAFETY: a NUL-terminated string of `bytes` bytes.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ,
                line.as_ptr().cast(),
                bytes,
            )
        }
    } else {
        // SAFETY: NUL-terminated names.
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            status => status,
        }
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(status as i32).to_string())
    }
}

/// Put `text` on the Windows clipboard.
fn copy_to_clipboard(owner: HWND, text: &str) {
    let text = wide(text);
    let bytes = text.len() * 2;
    // SAFETY: the memory is `bytes` long and handed to the clipboard on
    // success, freed otherwise; the clipboard is closed on every path.
    unsafe {
        if OpenClipboard(owner) == 0 {
            log::warn!("windows: settings: the clipboard is busy");
            return;
        }
        EmptyClipboard();
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        let mut handed = false;
        if !memory.is_null() {
            let to = GlobalLock(memory);
            if !to.is_null() {
                ptr::copy_nonoverlapping(text.as_ptr().cast::<u8>(), to.cast::<u8>(), bytes);
                GlobalUnlock(memory);
                handed = !SetClipboardData(CF_UNICODETEXT, memory).is_null();
            }
            if !handed {
                GlobalFree(memory);
            }
        }
        CloseClipboard();
    }
}

/// Ask while X clients are connected, then shut down for a restart.
fn restart(owner: HWND) {
    let Some((clients, link)) = CONTEXT.with_borrow(|context| {
        context
            .as_ref()
            .map(|c| (c.clients.load(Ordering::Acquire), Arc::clone(&c.link)))
    }) else {
        return;
    };
    if !super::app::confirm_quit(owner, clients, true) {
        return;
    }
    log::info!("windows: settings: restart");
    super::RESTART.store(true, Ordering::Release);
    link.request_shutdown();
}

/// A control's change.
fn command(hwnd: HWND, id: u16, notification: u16) {
    let mut store = store();
    match (id, u32::from(notification)) {
        (ID_COPY, BN_CLICKED) => {
            if let Some(name) = display_name() {
                copy_to_clipboard(hwnd, &name);
            }
        }
        (ID_ROOTLESS | ID_WINDOW, BN_CLICKED) => {
            let mode = if id == ID_WINDOW {
                Mode::Window
            } else {
                Mode::Rootless
            };
            store.set_string(KEY_MODE, mode.as_str());
            log::info!("windows: settings: mode = {}", mode.as_str());
        }
        (ID_SCALE, CBN_SELCHANGE) => {
            let control = item(hwnd, ID_SCALE);
            // SAFETY: the dialog's combo box.
            let at = unsafe { SendMessageW(control, CB_GETCURSEL, 0, 0) };
            let scale = usize::try_from(at)
                .ok()
                .and_then(|at| SCALES.get(at))
                .map_or(ScaleOption::Auto, |&(scale, _)| scale);
            store.set_string(KEY_SCALE, &scale_str(scale));
            log::info!("windows: settings: scale = {}", scale_str(scale));
        }
        (ID_RESTART, BN_CLICKED) => restart(hwnd),
        (ID_CLIPBOARD, BN_CLICKED) => {
            let on = checked(item(hwnd, ID_CLIPBOARD));
            store.set_bool(KEY_SYNC_CLIPBOARD, on);
            CONTEXT.with_borrow(|context| {
                if let Some(context) = context {
                    context.sync_clipboard.store(on, Ordering::Release);
                }
            });
            log::info!("windows: settings: {KEY_SYNC_CLIPBOARD} = {on}");
        }
        (ID_LOGIN, BN_CLICKED) => {
            let control = item(hwnd, ID_LOGIN);
            let on = checked(control);
            let result = set_login_item(on);
            log::info!("windows: settings: start at sign-in = {on}: {result:?}");
            refresh_login();
            if let Err(e) = result {
                let text = wide(&format!(
                    "{}\n\n{e}",
                    if on {
                        "yserver could not be set to start at sign-in."
                    } else {
                        "yserver could not be kept from starting at sign-in."
                    }
                ));
                // SAFETY: a live window and NUL-terminated strings.
                unsafe { MessageBoxW(hwnd, text.as_ptr(), w!("yserver"), MB_OK | MB_ICONWARNING) };
            }
        }
        (id, BN_CLICKED) if id == IDCANCEL as u16 => close(),
        _ => {}
    }
}

/// The dialog's control `id`.
fn item(hwnd: HWND, id: u16) -> HWND {
    // SAFETY: a plain query of this thread's window.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, i32::from(id)) }
}

/// Show the start-at-sign-in state as it is now.
fn refresh_login() {
    DIALOG.with_borrow(|dialog| {
        if let Some(dialog) = dialog {
            set_checked(dialog.login, login_item().view().checked);
        }
    });
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as u16;
            let notification = ((wparam >> 16) & 0xFFFF) as u16;
            command(hwnd, id, notification);
            0
        }
        WM_CTLCOLORSTATIC => {
            let control = lparam as HWND;
            let note = DIALOG
                .with_borrow(|dialog| dialog.as_ref().is_some_and(|d| d.notes.contains(&control)));
            let hdc = wparam as HDC;
            // SAFETY: the DC the control paints with, for this message.
            unsafe {
                if note {
                    SetTextColor(hdc, GetSysColor(COLOR_GRAYTEXT));
                }
                SetBkMode(hdc, TRANSPARENT as _);
                GetSysColorBrush(COLOR_BTNFACE) as LRESULT
            }
        }
        WM_ACTIVATE => {
            // The registry may have changed meanwhile.
            if (wparam & 0xFFFF) as u32 != WA_INACTIVE {
                refresh_login();
            }
            0
        }
        WM_CLOSE => {
            close();
            0
        }
        WM_DESTROY => {
            if let Some(dialog) = DIALOG.take() {
                // SAFETY: the font is ours and its controls are gone.
                unsafe { DeleteObject(dialog.font) };
            }
            0
        }
        // SAFETY: the default handling of a message for this window.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The message font, as the system's own dialogs use.
fn message_font() -> HFONT {
    // SAFETY: zeroed is a valid NONCLIENTMETRICSW; the call fills it.
    unsafe {
        let mut metrics: NONCLIENTMETRICSW = std::mem::zeroed();
        metrics.cbSize = u32::try_from(size_of::<NONCLIENTMETRICSW>()).unwrap_or(0);
        if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&raw mut metrics).cast::<c_void>(),
            0,
        ) == 0
        {
            return ptr::null_mut();
        }
        CreateFontIndirectW(&raw const metrics.lfMessageFont)
    }
}

/// Lays out controls top to bottom, measuring text in the dialog's font.
struct Builder {
    hwnd: HWND,
    font: HFONT,
    hdc: HDC,
    /// The text height.
    line: i32,
    notes: Vec<HWND>,
    /// The right edge of the widest row.
    right: i32,
}

const MARGIN: i32 = 12;
const GAP: i32 = 8;

impl Builder {
    fn text_width(&self, text: &str) -> i32 {
        let text: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: a DC with the font selected and a buffer of that length.
        unsafe {
            GetTextExtentPoint32W(
                self.hdc,
                text.as_ptr(),
                i32::try_from(text.len()).unwrap_or(0),
                &raw mut size,
            );
        }
        size.cx
    }

    #[allow(clippy::too_many_arguments)]
    fn control(
        &mut self,
        class: PCWSTR,
        text: &str,
        style: WINDOW_STYLE,
        id: u16,
        (x, y): (i32, i32),
        (width, height): (i32, i32),
    ) -> HWND {
        let text = wide(text);
        // SAFETY: a child of the dialog; the id is its menu parameter.
        let control = unsafe {
            CreateWindowExW(
                0,
                class,
                text.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                width,
                height,
                self.hwnd,
                usize::from(id) as _,
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            )
        };
        // SAFETY: a control of this thread; the font outlives it.
        unsafe { SendMessageW(control, WM_SETFONT, self.font as WPARAM, 0) };
        self.right = self.right.max(x + width);
        control
    }

    fn label(&mut self, text: &str, at: (i32, i32)) -> HWND {
        let width = self.text_width(text);
        self.control(w!("STATIC"), text, 0, 0, at, (width, self.line))
    }

    fn note(&mut self, text: &str, at: (i32, i32)) -> HWND {
        let note = self.label(text, at);
        self.notes.push(note);
        note
    }

    fn button(&mut self, text: &str, id: u16, style: WINDOW_STYLE, at: (i32, i32)) -> HWND {
        let width = (self.text_width(text) + 24).max(75);
        self.control(
            w!("BUTTON"),
            text,
            style | WS_TABSTOP,
            id,
            at,
            (width, self.line + 9),
        )
    }

    /// A check box or radio button: the box, a gap and the text.
    fn check(&mut self, text: &str, id: u16, style: WINDOW_STYLE, at: (i32, i32)) -> HWND {
        let width = self.text_width(text) + 24;
        self.control(w!("BUTTON"), text, style, id, at, (width, self.line + 4))
    }
}

fn create() -> std::io::Result<()> {
    let Some((set_by_argv, run_mode, run_scale, owner, sync)) = CONTEXT.with_borrow(|context| {
        context.as_ref().map(|c| {
            (
                c.set_by_argv,
                c.mode,
                c.scale,
                c.owner,
                c.sync_clipboard.load(Ordering::Acquire),
            )
        })
    }) else {
        return Err(std::io::Error::other("no settings context"));
    };
    let stored = crate::settings::Settings::load(&store());
    let mode = if set_by_argv.mode {
        run_mode
    } else {
        stored.mode
    };
    let scale = if set_by_argv.scale {
        run_scale
    } else {
        stored.scale
    };

    // SAFETY: the module handle of this executable.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let class = WNDCLASSEXW {
        cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        // SAFETY: system resources by predefined id.
        hIcon: unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) },
        // SAFETY: as above.
        hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) },
        // SAFETY: a system brush, never freed.
        hbrBackground: unsafe { GetSysColorBrush(COLOR_BTNFACE) },
        lpszMenuName: ptr::null(),
        lpszClassName: CLASS_NAME,
        hIconSm: ptr::null_mut(),
    };
    // SAFETY: a fully initialised class description; registering twice
    // fails harmlessly.
    unsafe { RegisterClassExW(&raw const class) };
    let style = WS_CAPTION | WS_SYSMENU;
    let ex_style = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
    // SAFETY: a registered class; sized and shown once its controls exist.
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            CLASS_NAME,
            w!("yserver Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            owner,
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let font = message_font();
    // SAFETY: the dialog's DC, released below.
    let hdc = unsafe { GetDC(hwnd) };
    // SAFETY: a live DC and font.
    let old_font = unsafe { SelectObject(hdc, font) };
    let mut b = Builder {
        hwnd,
        font,
        hdc,
        line: 0,
        notes: Vec::new(),
        right: 0,
    };
    let mut size = SIZE { cx: 0, cy: 0 };
    // SAFETY: as in `text_width`.
    unsafe { GetTextExtentPoint32W(hdc, w!("Xg"), 2, &raw mut size) };
    b.line = size.cy;
    let row = b.line + 12;
    let indent = MARGIN
        + ["X display:", "Mode:", "Scale:"]
            .iter()
            .map(|t| b.text_width(t))
            .max()
            .unwrap_or(0)
        + GAP;
    let check_lift = 2;
    let button_lift = 4;

    let mut y = MARGIN + button_lift;
    b.label("X display:", (MARGIN, y));
    let display_width = b
        .text_width("localhost:65535")
        .max(b.text_width("starting"));
    let display = b.control(
        w!("STATIC"),
        &display_text(),
        0,
        0,
        (indent, y),
        (display_width, b.line),
    );
    b.button(
        "Copy",
        ID_COPY,
        BS_PUSHBUTTON as WINDOW_STYLE,
        (indent + display_width + GAP, y - button_lift),
    );
    y += row + GAP;

    b.label("Mode:", (MARGIN, y));
    let rootless = b.check(
        "Rootless (a Windows window per X window)",
        ID_ROOTLESS,
        BS_AUTORADIOBUTTON as WINDOW_STYLE | WS_GROUP | WS_TABSTOP,
        (indent, y - check_lift),
    );
    y += row - 4;
    let window = b.check(
        "One window for the X screen",
        ID_WINDOW,
        BS_AUTORADIOBUTTON as WINDOW_STYLE,
        (indent, y - check_lift),
    );
    set_checked(
        if mode == Mode::Window {
            window
        } else {
            rootless
        },
        true,
    );
    if set_by_argv.mode {
        for radio in [rootless, window] {
            // SAFETY: a control of this thread.
            unsafe { EnableWindow(radio, 0) };
        }
        y += row - 4;
        b.note("set by command line", (indent, y));
    }
    y += row;

    b.label("Scale:", (MARGIN, y));
    let combo_width = SCALES
        .iter()
        .map(|&(_, title)| b.text_width(title))
        .max()
        .unwrap_or(0)
        + 40;
    let combo = b.control(
        w!("COMBOBOX"),
        "",
        CBS_DROPDOWNLIST as WINDOW_STYLE | WS_VSCROLL | WS_TABSTOP | WS_GROUP,
        ID_SCALE,
        (indent, y - button_lift),
        (combo_width, b.line * 8),
    );
    for (_, title) in SCALES {
        let title = wide(title);
        // SAFETY: the combo box copies the string.
        unsafe { SendMessageW(combo, CB_ADDSTRING, 0, title.as_ptr() as LPARAM) };
    }
    let at = SCALES.iter().position(|&(s, _)| s == scale).unwrap_or(0);
    // SAFETY: a control of this thread.
    unsafe { SendMessageW(combo, CB_SETCURSEL, at, 0) };
    if set_by_argv.scale {
        // SAFETY: a control of this thread.
        unsafe { EnableWindow(combo, 0) };
        b.note("set by command line", (indent + combo_width + GAP, y));
    }
    y += row + GAP;

    let (can_restart, restart_note) = restart_view(true, false);
    b.note(restart_note, (MARGIN, y));
    let restart = b.button(
        "Restart yserver",
        ID_RESTART,
        BS_PUSHBUTTON as WINDOW_STYLE,
        (MARGIN + b.text_width(restart_note) + GAP, y - button_lift),
    );
    // SAFETY: a control of this thread.
    unsafe { EnableWindow(restart, i32::from(can_restart)) };
    y += row + 2 * GAP;

    let clipboard = b.check(
        "Sync the clipboard with the Windows clipboard",
        ID_CLIPBOARD,
        BS_AUTOCHECKBOX as WINDOW_STYLE | WS_TABSTOP | WS_GROUP,
        (MARGIN, y),
    );
    set_checked(clipboard, sync);
    y += row;
    let login = b.check(
        "Start yserver when you sign in",
        ID_LOGIN,
        BS_AUTOCHECKBOX as WINDOW_STYLE | WS_TABSTOP,
        (MARGIN, y),
    );
    set_checked(login, login_item().view().checked);
    y += row + 2 * GAP;

    let close_width = (b.text_width("Close") + 24).max(75);
    let width = b.right.max(close_width) + MARGIN;
    b.button(
        "Close",
        IDCANCEL as u16,
        BS_DEFPUSHBUTTON as WINDOW_STYLE,
        (width - MARGIN - close_width, y),
    );
    let height = y + b.line + 9 + MARGIN;

    // SAFETY: restores the DC's font and releases it.
    unsafe {
        SelectObject(hdc, old_font);
        ReleaseDC(hwnd, hdc);
    }
    let notes = std::mem::take(&mut b.notes);
    DIALOG.set(Some(Dialog {
        hwnd,
        font,
        display,
        login,
        notes,
    }));

    let mut frame = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // SAFETY: writes the rectangle; no menu.
    unsafe { AdjustWindowRectEx(&raw mut frame, style, 0, ex_style) };
    let (frame_width, frame_height) = (frame.right - frame.left, frame.bottom - frame.top);
    // Centred over the owner, else on the primary screen's work area.
    let mut over = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: plain queries; writes the rectangle.
    let found = unsafe {
        if owner.is_null() {
            SystemParametersInfoW(
                windows_sys::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
                0,
                (&raw mut over).cast(),
                0,
            )
        } else {
            GetWindowRect(owner, &raw mut over)
        }
    } != 0;
    let (x, y) = if found {
        (
            over.left + (over.right - over.left - frame_width) / 2,
            over.top + (over.bottom - over.top - frame_height) / 2,
        )
    } else {
        (CW_USEDEFAULT, CW_USEDEFAULT)
    };
    // SAFETY: the dialog of this thread.
    unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            x,
            y,
            frame_width,
            frame_height,
            SWP_NOZORDER,
        );
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
    Ok(())
}
