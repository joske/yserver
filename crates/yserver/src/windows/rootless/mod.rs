//! Rootless mode: one Win32 window per X top-level. The core thread owns
//! the presenter and [`WinRootlessHost`]; the UI thread owns every window
//! ([`shell`]) and runs the message loop; the `DwmFlush` thread ticks the
//! frame pacer.
//!
//! Shutdown: the core loop returns, drops every Vulkan object and asks the
//! UI thread to quit (`ShellUi::quit`), which destroys the message window
//! and ends the message loop; `run` then returns the core's result.

mod shell;
mod window;
mod wndproc;

use std::{
    collections::{HashMap, HashSet},
    io, ptr,
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
};

use ash::vk;
use windows_sys::Win32::{
    Foundation::HWND,
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{DestroyWindow, DispatchMessageW, GetMessageW, MSG},
};
use yserver_core::rootless::{RootlessEvent, ToplevelId};

use super::{frame_clock::DwmTicker, input::ShellInput};
use crate::{
    kms::vk::device::VkContext,
    launch::LaunchOptions,
    rootless::{
        host::{CpuFrame, HandoffKind, RootlessHost},
        native::{pacing::FramePacer, space::GlobalSpace},
    },
    surface::{
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
    /// Windows with a Vulkan surface: freed at `surface_released`.
    surfaced: HashSet<ToplevelId>,
    hwnds: Arc<HwndTable>,
}

impl RootlessHost for WinRootlessHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> HandoffKind {
        HandoffKind::VulkanSurface
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
    // One X pixel per pixel of a DPI-unaware process.
    let displays: Vec<_> = window::monitors().into_iter().map(window::native).collect();
    let space = GlobalSpace::new(&displays, 1.0, MAX_ROOT_SIDE);
    log::info!(
        "windows: rootless over {} display(s), root {}x{}",
        displays.len(),
        space.root().0,
        space.root().1
    );
    let link = Arc::new(ShellLink::default());
    let clock = Arc::new(FrameClock::new(Arc::clone(&link)));
    let pacer = Arc::new(FramePacer::new(Arc::clone(&link)));
    {
        let pacer = Arc::clone(&pacer);
        clock.set_tick_hook(move || pacer.tick());
    }
    let input = Rc::new(ShellInput::new(Arc::clone(&link), space.root()));
    let hwnds = Arc::new(HwndTable::default());
    shell::install(
        space,
        Arc::clone(&link),
        Arc::clone(&hwnds),
        input,
        message_window,
    );
    let ticker = DwmTicker::start(Arc::clone(&clock))?;
    let spec = RootlessSpec {
        surface_extension: ash::khr::win32_surface::NAME,
        root: space.root(),
        clock,
        link,
        ui: Arc::new(RootlessUi {
            message_window: message_window as usize,
        }),
        clients: Arc::default(),
    };
    let host = WinRootlessHost {
        pacer,
        surfaced: HashSet::new(),
        hwnds,
    };
    // TODO(windows): bridge CLIPBOARD with the Windows clipboard.
    let core = thread::Builder::new()
        .name("yserver-core".into())
        .spawn(move || server::run_rootless(opts, spec, Box::new(host), None))?;

    // No `TranslateMessage`: keys go to X by scan code, not as characters.
    // SAFETY: zeroed is a valid MSG.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: this thread's queue; writes `msg`.
    while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: a message `GetMessageW` returned.
        unsafe { DispatchMessageW(&raw const msg) };
    }
    drop(ticker);
    shell::uninstall();
    core.join()
        .map_err(|_| io::Error::other("the core thread panicked"))?
}
