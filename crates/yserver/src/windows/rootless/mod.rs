//! Rootless mode: one Win32 window per X top-level. The core thread owns
//! the presenter and [`WinRootlessHost`]; the UI thread owns every window
//! ([`shell`]) and runs the message loop; the `DwmFlush` thread ticks the
//! frame pacer.
//!
//! Shutdown: the tray icon's Exit asks the core to shut down
//! (`super::tray`); the
//! core loop returns, drops every Vulkan object and asks the UI thread to
//! quit (`ShellUi::quit`), which destroys the message window
//! and ends the message loop; `run` then returns the core's result.

mod shell;
mod window;
mod wndproc;

use std::{
    collections::{HashMap, HashSet},
    io, ptr,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
    thread,
};

use ash::vk;
use windows_sys::Win32::{
    Foundation::HWND,
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{DestroyWindow, DispatchMessageW, GetMessageW, MSG},
};
use yserver_core::rootless::{RootlessEvent, ToplevelId};

use super::{
    clipboard::WindowsClipboard, frame_clock::DwmTicker, input::ShellInput, scale, settings_dialog,
    tray,
};
use crate::{
    kms::vk::device::VkContext,
    launch::LaunchOptions,
    rootless::{
        host::{CpuFrame, HandoffKind, RootlessHost},
        native::{pacing::FramePacer, space::GlobalSpace},
    },
    settings::Mode,
    surface::{
        clipboard::HostClipboard,
        clock::FrameClock,
        link::ShellLink,
        server::{self, RootlessSpec},
        ui::{CursorImage, ShellUi},
    },
};

/// The largest root side offered (the surface platform's limit).
const MAX_ROOT_SIDE: u16 = 8192;

/// Every top-level's window, as an integer for the core thread, set on the
/// UI thread before `SetupDone`.
#[derive(Default)]
struct HwndTable(Mutex<HashMap<ToplevelId, usize>>);

impl HwndTable {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<ToplevelId, usize>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn insert(&self, id: ToplevelId, hwnd: HWND) {
        self.map().insert(id, hwnd as usize);
    }
}

/// The host on the core thread; every window change goes to the UI thread.
struct WinRootlessHost {
    pacer: Arc<FramePacer>,
    /// Device pixels per X pixel per axis.
    factor: u32,
    /// Windows with a Vulkan surface: freed at `surface_released`.
    surfaced: HashSet<ToplevelId>,
    hwnds: Arc<HwndTable>,
}

impl RootlessHost for WinRootlessHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> HandoffKind {
        HandoffKind::VulkanSurface
    }

    fn copy_scale(&self) -> u32 {
        self.factor
    }

    fn create_surface(&mut self, id: ToplevelId, vk: &VkContext) -> io::Result<vk::SurfaceKHR> {
        let hwnd = self
            .hwnds
            .map()
            .get(&id)
            .copied()
            .ok_or_else(|| io::Error::other("no window for the top-level"))?;
        let loader = ash::khr::win32_surface::Instance::new(&vk.entry, &vk.instance);
        // SAFETY: the module handle of this executable.
        let instance = unsafe { GetModuleHandleW(ptr::null()) } as vk::HINSTANCE;
        let info = vk::Win32SurfaceCreateInfoKHR::default()
            .hinstance(instance)
            .hwnd(hwnd as vk::HWND);
        // SAFETY: the UI thread keeps the window until `surface_released`;
        // an instance with the extension.
        let surface = unsafe { loader.create_win32_surface(&info, None) }
            .map_err(|e| io::Error::other(format!("vkCreateWin32SurfaceKHR: {e:?}")))?;
        self.surfaced.insert(id);
        Ok(surface)
    }

    fn submit_cpu(&mut self, frame: CpuFrame) {
        // Never chosen: every window is a Vulkan surface.
        drop(frame);
    }

    fn first_frame(&mut self, id: ToplevelId) {
        shell::on_ui(move |shell| shell.first_frame(id));
    }

    fn on_events(&mut self, events: &[RootlessEvent]) {
        // Unmapped before it got a surface: no `surface_released` follows.
        let freed: Vec<ToplevelId> = events
            .iter()
            .filter_map(|e| match e {
                RootlessEvent::Unmapped { id } if !self.surfaced.contains(id) => Some(*id),
                _ => None,
            })
            .collect();
        for id in &freed {
            self.hwnds.map().remove(id);
        }
        let events = events.to_vec();
        shell::on_ui(move |shell| {
            shell.apply(&events);
            for id in freed {
                shell.release(id);
            }
        });
    }

    fn frame_wanted(&mut self, id: ToplevelId) {
        self.pacer.want(id);
    }

    fn surface_released(&mut self, id: ToplevelId) {
        self.surfaced.remove(&id);
        self.hwnds.map().remove(&id);
        shell::on_ui(move |shell| shell.release(id));
    }
}

/// `ShellUi` of a rootless shell: there is no root window.
struct RootlessUi {
    /// The message window, as an integer: posting to it is thread-safe.
    message_window: usize,
}

impl ShellUi for RootlessUi {
    fn set_display(&self, display: u16) {
        log::info!("windows: rootless, DISPLAY=:{display}");
        shell::on_ui(move |_| {
            tray::set_display(display);
            settings_dialog::set_display(display);
        });
    }

    fn quit(&self, _failed: bool) {
        // `run` returns the core's result, failed or not.
        let hwnd = self.message_window;
        shell::on_ui(move |_| {
            // SAFETY: the message window of this thread; `WM_DESTROY` ends
            // the message loop.
            unsafe { DestroyWindow(hwnd as HWND) };
        });
    }

    fn set_cursor(&self, cursor: Option<CursorImage>) {
        shell::on_ui(move |shell| shell.set_cursor(cursor));
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        shell::on_ui(move |shell| shell.warp(x, y));
    }

    fn set_content_size(&self, width: u16, height: u16, _token: u64) {
        log::debug!("windows: rootless ignores a window size {width}x{height}");
    }
}

/// Run rootless mode until the server quits: the display space, the shell,
/// the frame clock, the core thread and the message loop.
///
/// # Errors
///
/// A window class, the message window or a thread could not be created,
/// or the server failed (see [`server::run_rootless`]).
pub(super) fn run(opts: LaunchOptions) -> io::Result<()> {
    window::register(Some(wndproc::toplevel), Some(wndproc::message))?;
    let message_window = window::create_message_window()?;
    // One scale for every monitor (as macOS, D2): the primary's. Monitors
    // are in device pixels; one X pixel covers `factor`x`factor` of them.
    let primary = scale::at_dpi(scale::primary_dpi());
    let factor = primary.factor;
    let displays: Vec<_> = window::monitors().into_iter().map(window::native).collect();
    let space = GlobalSpace::new(&displays, 1.0 / f64::from(factor), MAX_ROOT_SIDE);
    log::info!(
        "windows: rootless over {} display(s), root {}x{}, {} device pixels per X pixel, \
         {} X pixels per logical pixel",
        displays.len(),
        space.root().0,
        space.root().1,
        factor,
        primary.xpp
    );
    let link = Arc::new(ShellLink::default());
    let clock = Arc::new(FrameClock::new(Arc::clone(&link)));
    let pacer = Arc::new(FramePacer::new(Arc::clone(&link)));
    {
        let pacer = Arc::clone(&pacer);
        clock.set_tick_hook(move || pacer.tick());
    }
    let input = Rc::new(ShellInput::new(Arc::clone(&link), space.root()));
    input.set_factor(factor);
    super::cursor::set_factor(factor, false);
    let hwnds = Arc::new(HwndTable::default());
    shell::install(
        space,
        factor,
        Arc::clone(&link),
        Arc::clone(&hwnds),
        input,
        message_window,
    );
    let clients = Arc::new(AtomicUsize::new(0));
    let sync_clipboard = Arc::new(AtomicBool::new(opts.live.sync_clipboard));
    tray::install(message_window, Arc::clone(&link), Arc::clone(&clients));
    settings_dialog::install(settings_dialog::Context {
        link: Arc::clone(&link),
        clients: Arc::clone(&clients),
        sync_clipboard: Arc::clone(&sync_clipboard),
        set_by_argv: opts.set_by_argv,
        mode: Mode::Rootless,
        scale: opts.scale,
        owner: ptr::null_mut(),
    });
    let ticker = DwmTicker::start(Arc::clone(&clock))?;
    let spec = RootlessSpec {
        surface_extension: ash::khr::win32_surface::NAME,
        root: space.root(),
        clock,
        link,
        ui: Arc::new(RootlessUi {
            message_window: message_window as usize,
        }),
        clients,
    };
    let host = WinRootlessHost {
        pacer,
        factor,
        surfaced: HashSet::new(),
        hwnds,
    };
    let clipboard = HostClipboard {
        pasteboard: Box::new(WindowsClipboard::new(message_window)),
        sync: sync_clipboard,
    };
    let core = thread::Builder::new()
        .name("yserver-core".into())
        .spawn(move || server::run_rootless(opts, spec, Box::new(host), Some(clipboard)))?;

    // No `TranslateMessage`: keys go to X by scan code, not as characters.
    // SAFETY: zeroed is a valid MSG.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: this thread's queue; writes `msg`.
    while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {
        if settings_dialog::is_dialog_message(&msg) {
            continue;
        }
        // SAFETY: a message `GetMessageW` returned.
        unsafe { DispatchMessageW(&raw const msg) };
    }
    drop(ticker);
    settings_dialog::uninstall();
    shell::uninstall();
    core.join()
        .map_err(|_| io::Error::other("the core thread panicked"))?
}
