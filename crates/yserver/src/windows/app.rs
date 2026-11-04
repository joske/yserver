//! The Win32 application: one window, its message loop on the calling
//! thread and the core loop on its own thread. The UI thread never touches
//! Vulkan; the core's `ShellUi` commands reach it as a posted message.
//!
//! The system menu (a right click on the title bar) and the tray icon open the
//! Settings.
//!
//! Shutdown: the close button asks first while X clients are connected,
//! then asks the core to shut down; the core loop returns, drops every
//! Vulkan object and asks the UI thread to quit (`ShellUi::quit`), which
//! destroys the window and ends the message loop. `run` then returns the
//! core's result.

use std::{
    cell::{Cell, RefCell},
    io, ptr,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::ValidateRect,
        UI::WindowsAndMessaging::{
            AppendMenuW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageTime,
            GetMessageW, GetSystemMenu, HTCLIENT, IDOK, MB_ICONWARNING, MB_OKCANCEL, MF_SEPARATOR,
            MF_STRING, MINMAXINFO, MSG, MessageBoxW, PostMessageW, PostQuitMessage, SC_KEYMENU,
            SIZE_MINIMIZED, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow,
            SetWindowPos, ShowWindow, WM_APP, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE,
            WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_GETMINMAXINFO,
            WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
            WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN,
            WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SIZE, WM_SYSCOMMAND, WM_SYSKEYDOWN,
            WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    },
    core::w,
};

use super::{
    clipboard::WindowsClipboard,
    frame_clock::DwmTicker,
    input::{self, RawKeyMessage, ShellInput},
    resize::Sizer,
    scale::{self, Metrics},
    settings_dialog, tray, window,
};
use crate::{
    launch::LaunchOptions,
    settings::Mode,
    surface::{
        clipboard::HostClipboard,
        clock::FrameClock,
        link::{ShellEvent, ShellLink},
        resize::{MIN_SCREEN, Size},
        server::{self, WindowSpec},
        ui::{CursorImage, ShellUi},
    },
};

/// The core's commands are queued (`UiQueue`).
const WM_UI_COMMANDS: u32 = WM_APP + 1;
/// A frame-clock tick a waiting size asked for.
const WM_RESIZE_TICK: u32 = WM_APP + 2;
/// The system menu's Settings item (the low four bits are the system's).
const SETTINGS_COMMAND: usize = 0x0100;

/// What the core thread asks of the UI thread.
enum UiCommand {
    SetDisplay(u16),
    Quit,
    SetCursor(Option<CursorImage>),
    Warp(i32, i32),
    SetContentSize(u16, u16, u64),
}

/// `ShellUi` for the core thread: each command is queued and the window
/// told by a posted message.
struct UiQueue {
    /// The window, as an integer: posting to it is thread-safe.
    hwnd: usize,
    commands: Mutex<Vec<UiCommand>>,
}

impl UiQueue {
    fn push(&self, command: UiCommand) {
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(command);
        post(self.hwnd, WM_UI_COMMANDS);
    }

    fn take(&self) -> Vec<UiCommand> {
        std::mem::take(
            &mut *self
                .commands
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl ShellUi for UiQueue {
    fn set_display(&self, display: u16) {
        self.push(UiCommand::SetDisplay(display));
    }

    fn quit(&self, _failed: bool) {
        // `run` returns the core's result, failed or not.
        self.push(UiCommand::Quit);
    }

    fn set_cursor(&self, cursor: Option<CursorImage>) {
        self.push(UiCommand::SetCursor(cursor));
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        self.push(UiCommand::Warp(x, y));
    }

    fn set_content_size(&self, width: u16, height: u16, token: u64) {
        self.push(UiCommand::SetContentSize(width, height, token));
    }
}

/// Ask whether to quit, or to `restart`, while `clients` X clients are
/// connected; `true` without asking when none is.
pub(super) fn confirm_quit(owner: HWND, clients: usize, restart: bool) -> bool {
    if clients == 0 {
        return true;
    }
    let noun = if clients == 1 { "client" } else { "clients" };
    let verb = if restart { "Restarting" } else { "Quitting" };
    let text: Vec<u16> =
        format!("{clients} X {noun} connected\n\n{verb} yserver disconnects them.")
            .encode_utf16()
            .chain([0])
            .collect();
    // SAFETY: a live window (or none) and NUL-terminated strings.
    let answer = unsafe {
        MessageBoxW(
            owner,
            text.as_ptr(),
            windows_sys::core::w!("yserver"),
            MB_OKCANCEL | MB_ICONWARNING,
        )
    };
    answer == IDOK
}

/// Post `message` to the window `hwnd` from any thread.
fn post(hwnd: usize, message: u32) {
    // SAFETY: posting to a window handle is thread-safe; a destroyed
    // window just fails.
    unsafe { PostMessageW(hwnd as HWND, message, 0, 0) };
}

/// The window's state on the UI thread.
struct Shell {
    hwnd: HWND,
    link: Arc<ShellLink>,
    ui: Arc<UiQueue>,
    /// Established X clients, kept by the core.
    clients: Arc<AtomicUsize>,
    /// The token of the core's last window resize request (0: none).
    resize_request: Arc<AtomicU64>,
    input: ShellInput,
    sizer: RefCell<Sizer>,
    minimized: Cell<bool>,
    /// The display configuration changed since the metrics were read.
    display_changed: Cell<bool>,
    /// The user already confirmed closing.
    close_confirmed: Cell<bool>,
}

thread_local! {
    static SHELL: RefCell<Option<Rc<Shell>>> = const { RefCell::new(None) };
}

fn shell() -> Option<Rc<Shell>> {
    SHELL.with_borrow(Clone::clone)
}

impl Shell {
    /// The client area changed (`WM_SIZE`, `lparam` its new size).
    fn sized(&self, kind: WPARAM, lparam: LPARAM) {
        if kind == SIZE_MINIMIZED as WPARAM {
            if !self.minimized.replace(true) {
                self.link.post(ShellEvent::Visible(false));
            }
            return;
        }
        if self.minimized.replace(false) {
            self.link.post(ShellEvent::Visible(true));
        }
        let width = i32::from(u16::try_from(lparam & 0xFFFF).unwrap_or(0));
        let height = i32::from(u16::try_from((lparam >> 16) & 0xFFFF).unwrap_or(0));
        self.sizer
            .borrow_mut()
            .resized(&self.input, (width, height));
    }

    /// `WM_DPICHANGED`: the window moved to a monitor of another DPI, or
    /// the scaling changed. The window takes the suggested rect (the same
    /// logical size, as macOS keeps a window's points), the cursor and the
    /// core the new scale.
    fn dpi_changed(&self, wparam: WPARAM, suggested: RECT) {
        let dpi = u32::try_from(wparam & 0xFFFF).unwrap_or(0);
        let metrics = Metrics::at_rect(dpi, suggested);
        log::debug!(
            "windows: DPI {dpi}, factor {}, {:?}",
            metrics.factor(),
            metrics.scale
        );
        // Before the resize: `WM_SIZE` converts with them.
        self.sizer.borrow_mut().set_metrics(metrics);
        // SAFETY: the window of this thread; `WM_SIZE` arrives before this
        // returns, with no borrow held.
        unsafe {
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.scale_changed(metrics);
    }

    /// The window may be on another monitor, or the monitor changed: new
    /// millimetres.
    fn monitor_changed(&self) {
        let old = self.sizer.borrow().metrics();
        if scale::monitor_of(self.hwnd) == old.monitor
            && scale::window_dpi(self.hwnd) == old.dpi
            && !self.display_changed.get()
        {
            return;
        }
        self.display_changed.set(false);
        // Reads the EDID: only on a change.
        let metrics = Metrics::of_window(self.hwnd);
        self.sizer.borrow_mut().set_metrics(metrics);
        self.scale_changed(metrics);
    }

    /// The cursor and the core follow `metrics`.
    fn scale_changed(&self, metrics: Metrics) {
        self.input.set_factor(metrics.factor());
        super::cursor::set_factor(metrics.factor(), super::cursor::pointer_inside(self.hwnd));
        let client = window::client_size(self.hwnd);
        self.sizer.borrow_mut().metrics_changed(&self.input, client);
    }

    /// The close button: ask while X clients are connected, then shut down.
    fn close(&self) {
        let clients = self.clients.load(Ordering::Acquire);
        // The box runs a modal loop that may re-enter the window procedure,
        // with no borrow held.
        if !self.close_confirmed.get() && !confirm_quit(self.hwnd, clients, false) {
            return;
        }
        self.close_confirmed.set(true);
        self.link.request_shutdown();
    }

    /// Run the core's queued commands.
    fn run_commands(&self) {
        for command in self.ui.take() {
            match command {
                UiCommand::SetDisplay(display) => {
                    window::set_title(self.hwnd, display);
                    tray::set_display(display);
                    settings_dialog::set_display(display);
                }
                UiCommand::Quit => {
                    // SAFETY: the window of this thread; `WM_DESTROY` ends
                    // the message loop.
                    unsafe { DestroyWindow(self.hwnd) };
                }
                UiCommand::SetCursor(cursor) => super::cursor::set(self.hwnd, cursor.as_ref()),
                UiCommand::Warp(x, y) => super::cursor::warp(self.hwnd, x, y),
                UiCommand::SetContentSize(width, height, token) => {
                    self.set_content_size((width, height), token);
                }
            }
        }
    }

    /// Size the client area to `size` for the core's request `token`.
    fn set_content_size(&self, size: Size, token: u64) {
        // Before the resize: `WM_SIZE` holds back its own report.
        self.resize_request.store(token, Ordering::Release);
        let metrics = self.sizer.borrow().metrics();
        let (width, height) = window::frame_size(metrics.scale.to_device(size), metrics.dpi);
        // SAFETY: the window of this thread; `WM_SIZE` arrives before this
        // returns, with no borrow held.
        unsafe {
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let client = window::client_size(self.hwnd);
        self.sizer.borrow_mut().answer_request(&self.input, client);
    }
}

/// The window procedure.
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_GETMINMAXINFO {
        let dpi = scale::window_dpi(hwnd);
        let (width, height) = window::frame_size(scale::at_dpi(dpi).to_device(MIN_SCREEN), dpi);
        // SAFETY: `lparam` is the system's MINMAXINFO for this message.
        let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
        info.ptMinTrackSize.x = width;
        info.ptMinTrackSize.y = height;
        return 0;
    }
    let Some(shell) = shell() else {
        // SAFETY: the default handling of a message for this window.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    if tray::message(message, lparam) {
        return 0;
    }
    match message {
        WM_UI_COMMANDS => shell.run_commands(),
        WM_RESIZE_TICK => shell.sizer.borrow_mut().tick(&shell.input),
        WM_CLOSE => shell.close(),
        WM_DESTROY => {
            tray::remove();
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
        WM_SIZE => shell.sized(wparam, lparam),
        WM_ENTERSIZEMOVE => shell.sizer.borrow_mut().live_started(),
        WM_EXITSIZEMOVE => {
            shell.sizer.borrow_mut().live_ended(&shell.input);
            shell.monitor_changed();
        }
        WM_DPICHANGED => {
            // SAFETY: `lparam` is the suggested outer rect for this message.
            let suggested = unsafe { *(lparam as *const RECT) };
            shell.dpi_changed(wparam, suggested);
        }
        WM_DISPLAYCHANGE => {
            shell.display_changed.set(true);
            shell.monitor_changed();
        }
        // Vulkan paints the client area.
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            // SAFETY: the window of this thread.
            unsafe { ValidateRect(hwnd, ptr::null()) };
        }
        WM_SETCURSOR if u32::try_from(lparam & 0xFFFF).ok() == Some(HTCLIENT) => {
            super::cursor::apply_current();
            return 1;
        }
        WM_SETFOCUS => shell.input.focus_gained(),
        WM_KILLFOCUS => shell.input.focus_lost(),
        // Not to `DefWindowProcW`: Alt and F10 would open the (absent)
        // window menu, Alt+F4 would close; X gets them all.
        WM_SYSCOMMAND if wparam & 0xFFF0 == SETTINGS_COMMAND => settings_dialog::show(),
        // The keyboard's way into the window menu (Alt, F10, Alt+Space):
        // X gets those keys, so the menu opens only from the title bar.
        WM_SYSCOMMAND if wparam & 0xFFF0 == SC_KEYMENU as WPARAM => {}
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            // SAFETY: a plain query of the message being handled.
            let time = unsafe { GetMessageTime() } as u32;
            let raw = RawKeyMessage::new(wparam, lparam, pressed, time);
            shell.input.key(hwnd, raw);
        }
        WM_MOUSEMOVE => shell.input.motion(input::client_position(lparam)),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP => {
            let (number, pressed) = match message {
                WM_LBUTTONDOWN => (0, true),
                WM_LBUTTONUP => (0, false),
                WM_RBUTTONDOWN => (1, true),
                WM_RBUTTONUP => (1, false),
                WM_MBUTTONDOWN => (2, true),
                _ => (2, false),
            };
            let at = input::client_position(lparam);
            shell.input.button(hwnd, number, pressed, at, wparam);
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            if let Some(number) = input::x_button(wparam) {
                let at = input::client_position(lparam);
                let pressed = message == WM_XBUTTONDOWN;
                shell.input.button(hwnd, number, pressed, at, wparam);
            }
            return 1;
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16;
            shell.input.wheel(delta, message == WM_MOUSEHWHEEL);
        }
        // SAFETY: the default handling of a message for this window.
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
    0
}

/// Run the window and the core thread until the server quits.
///
/// # Errors
///
/// The window or a thread could not be created, or the server failed (see
/// [`server::run_windowed`]).
pub(super) fn run(opts: LaunchOptions) -> io::Result<()> {
    let size = opts.geometry.unwrap_or(server::DEFAULT_SIZE);
    // At most the work area: a 2x factor doubles the window.
    let primary = Metrics::of_primary();
    let start = window::max_content_size(&primary)
        .map_or(size, |max| (size.0.min(max.0), size.1.min(max.1)));
    let hwnd = window::create(start, &primary, Some(wndproc))?;
    // The monitor the window landed on.
    let metrics = Metrics::of_window(hwnd);
    log::info!(
        "windows: {} DPI, {} device pixels per X pixel, {} X pixels per logical pixel",
        metrics.dpi,
        metrics.factor(),
        metrics.scale.xpp
    );
    let link = Arc::new(ShellLink::default());
    let resize_request = Arc::new(AtomicU64::new(0));
    let tick_wanted = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(FrameClock::new(Arc::clone(&link)));
    {
        let wanted = Arc::clone(&tick_wanted);
        let hwnd = hwnd as usize;
        clock.set_tick_hook(move || {
            if wanted.swap(false, Ordering::AcqRel) {
                post(hwnd, WM_RESIZE_TICK);
            }
        });
    }
    let ui = Arc::new(UiQueue {
        hwnd: hwnd as usize,
        commands: Mutex::new(Vec::new()),
    });
    let sizer = Sizer::new(
        size,
        metrics,
        Arc::clone(&link),
        Arc::clone(&resize_request),
        tick_wanted,
    );
    let output_mm = metrics.mm(size);
    let shell = Rc::new(Shell {
        hwnd,
        link: Arc::clone(&link),
        ui: Arc::clone(&ui),
        clients: Arc::default(),
        resize_request,
        input: ShellInput::new(Arc::clone(&link), size),
        sizer: RefCell::new(sizer),
        minimized: Cell::new(false),
        display_changed: Cell::new(false),
        close_confirmed: Cell::new(false),
    });
    shell.input.set_factor(metrics.factor());
    super::cursor::set_factor(metrics.factor(), false);
    SHELL.set(Some(Rc::clone(&shell)));
    let sync_clipboard = Arc::new(AtomicBool::new(opts.live.sync_clipboard));
    tray::install(hwnd, Arc::clone(&link), Arc::clone(&shell.clients));
    settings_dialog::install(settings_dialog::Context {
        link: Arc::clone(&link),
        clients: Arc::clone(&shell.clients),
        sync_clipboard: Arc::clone(&sync_clipboard),
        set_by_argv: opts.set_by_argv,
        mode: Mode::Window,
        scale: opts.scale,
        owner: hwnd,
    });
    // SAFETY: the window's own menu copy; the strings are NUL-terminated.
    unsafe {
        let menu = GetSystemMenu(hwnd, 0);
        if !menu.is_null() {
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            AppendMenuW(menu, MF_STRING, SETTINGS_COMMAND, w!("Settings…"));
        }
    }
    // The window may have come out smaller than asked (a small screen).
    let client = window::client_size(hwnd);
    shell.sizer.borrow_mut().resized(&shell.input, client);
    // SAFETY: the window of this thread.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }

    let ticker = DwmTicker::start(Arc::clone(&clock))?;
    let spec = WindowSpec {
        surface: window::win32_surface(hwnd),
        clock,
        link,
        ui,
        clients: Arc::clone(&shell.clients),
        max_output: window::max_content_size(&metrics),
        output_mm,
        copy_scale: metrics.factor(),
        clipboard: Some(HostClipboard {
            pasteboard: Box::new(WindowsClipboard::new(hwnd)),
            sync: sync_clipboard,
        }),
    };
    let core = thread::Builder::new()
        .name("yserver-core".into())
        .spawn(move || server::run_windowed(opts, spec))?;

    // No `TranslateMessage`: keys go to X by scan code, not as characters.
    // SAFETY: zeroed is a valid MSG.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: this thread's queue; writes `msg`.
    while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {
        if super::settings_dialog::is_dialog_message(&msg) {
            continue;
        }
        // SAFETY: a message `GetMessageW` returned.
        unsafe { DispatchMessageW(&raw const msg) };
    }
    drop(ticker);
    settings_dialog::uninstall();
    SHELL.set(None);
    core.join()
        .map_err(|_| io::Error::other("the core thread panicked"))?
}
