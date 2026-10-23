//! Rootless mode: one `NSWindow` per X top-level (plan RL2 Task 9). The core
//! thread owns the presenter and [`MacRootlessHost`]; the main thread owns
//! every window ([`shell`]); the display link ticks the frame pacer.

mod delegate;
mod shell;
mod view;
mod window;

use std::{
    collections::{HashMap, HashSet},
    io,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
    thread,
};

use ash::vk;
use dispatch2::DispatchQueue;
use objc2::{ClassType, MainThreadMarker};
use objc2_app_kit::{NSScreen, NSView};
use objc2_foundation::{NSObjectProtocol, NSSize};
use objc2_quartz_core::{CAMetalLayer, CATransaction};
use yserver_core::rootless::{RootlessEvent, ToplevelId};

use super::{
    app::{QuitState, TickSource, quit_app},
    input::ShellInput,
    scale::Metrics,
};
use crate::{
    kms::vk::device::VkContext,
    launch::LaunchOptions,
    rootless::{
        host::{CpuFrame, HandoffKind, RootlessHost},
        native::{
            pacing::FramePacer,
            space::{GlobalSpace, NativeRect},
        },
    },
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

/// A window's layer, for `vkCreateMetalSurfaceEXT` and its drawable size on
/// the core thread.
#[derive(Clone, Copy)]
struct LayerPtr(*const CAMetalLayer);

// SAFETY: the main thread keeps the layer retained until the core thread
// released its surface (`surface_released`) and removed this entry.
unsafe impl Send for LayerPtr {}

/// Every top-level's layer, set on the main thread before `SetupDone`.
#[derive(Default)]
pub(super) struct LayerTable(Mutex<HashMap<ToplevelId, LayerPtr>>);

impl LayerTable {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<ToplevelId, LayerPtr>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn insert(&self, id: ToplevelId, layer: &CAMetalLayer) {
        self.map().insert(id, LayerPtr(std::ptr::from_ref(layer)));
    }

    fn get(&self, id: ToplevelId) -> Option<LayerPtr> {
        self.map().get(&id).copied()
    }

    fn remove(&self, id: ToplevelId) {
        self.map().remove(&id);
    }
}

/// The host on the core thread; every window change goes to the main queue.
struct MacRootlessHost {
    layers: Arc<LayerTable>,
    pacer: Arc<FramePacer>,
    /// Windows with a Vulkan surface: freed at `surface_released`.
    surfaced: HashSet<ToplevelId>,
}

impl RootlessHost for MacRootlessHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> HandoffKind {
        HandoffKind::VulkanSurface
    }

    fn create_surface(&mut self, id: ToplevelId, vk: &VkContext) -> io::Result<vk::SurfaceKHR> {
        let layer = self
            .layers
            .get(id)
            .ok_or_else(|| io::Error::other("no layer for the window"))?;
        let loader = ash::ext::metal_surface::Instance::new(&vk.entry, &vk.instance);
        let info = vk::MetalSurfaceCreateInfoEXT::default().layer(layer.0.cast());
        // SAFETY: a live CAMetalLayer and an instance with the extension.
        let surface = unsafe { loader.create_metal_surface(&info, None) }
            .map_err(|e| io::Error::other(format!("vkCreateMetalSurfaceEXT: {e:?}")))?;
        self.surfaced.insert(id);
        Ok(surface)
    }

    fn submit_cpu(&mut self, frame: CpuFrame) {
        // Never chosen: every window is a Vulkan surface.
        drop(frame);
    }

    fn first_frame(&mut self, id: ToplevelId) {
        shell::on_main(move |shell| shell.first_frame(id));
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
            self.layers.remove(*id);
        }
        let events = events.to_vec();
        shell::on_main(move |shell| {
            shell.apply(&events);
            for id in freed {
                shell.release(id);
            }
        });
    }

    fn frame_wanted(&mut self, id: ToplevelId) {
        self.pacer.want(id);
    }

    fn surface_resizing(&mut self, id: ToplevelId, extent: (u32, u32)) {
        let Some(layer) = self.layers.get(id) else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // SAFETY: retained by the main thread until `surface_released`.
        unsafe { &*layer.0 }.setDrawableSize(NSSize::new(f64::from(extent.0), f64::from(extent.1)));
        CATransaction::commit();
    }

    fn surface_rebuilt(&mut self, _id: ToplevelId) {
        // The driver's layer changes, made on this thread, need a commit.
        CATransaction::flush();
    }

    fn surface_released(&mut self, id: ToplevelId) {
        self.surfaced.remove(&id);
        self.layers.remove(id);
        shell::on_main(move |shell| shell.release(id));
    }
}

/// `ShellUi` of a rootless shell: there is no root window.
struct RootlessUi {
    quit: Arc<QuitState>,
}

impl ShellUi for RootlessUi {
    fn set_display(&self, display: u16) {
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            super::display::announce(mtm, display);
        });
    }

    fn quit(&self, failed: bool) {
        quit_app(&self.quit, failed);
    }

    fn set_cursor(&self, cursor: Option<CursorImage>) {
        shell::on_main(move |shell| shell.set_cursor(cursor));
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        shell::on_main(move |shell| shell.warp(x, y));
    }

    fn set_content_size(&self, width: u16, height: u16, _token: u64) {
        log::debug!("macos: rootless ignores a window size {width}x{height}");
    }
}

/// Whether `view` is a top-level's view.
pub(super) fn is_toplevel_view(content: &NSView) -> bool {
    content.isKindOfClass(view::RootlessView::class())
}

/// The app became active: X's stacking follows the native order.
pub(super) fn app_activated() {
    shell::on_main(shell::RootlessShell::sync_stacking);
}

/// Another application became active.
pub(super) fn app_deactivated() {
    shell::on_main(shell::RootlessShell::app_deactivated);
}

/// Start rootless mode on the main thread: the display space, the shell,
/// the display link and the core thread. The tick source must be kept; the
/// input takes the app's keys (`ShellInput::install_key_monitor`).
///
/// # Errors
///
/// The frame clock or the core thread could not start.
pub(super) fn start(
    mtm: MainThreadMarker,
    opts: LaunchOptions,
    link: Arc<ShellLink>,
    quit: Arc<QuitState>,
    clients: Arc<AtomicUsize>,
    sync_clipboard: Arc<AtomicBool>,
) -> io::Result<(TickSource, Rc<ShellInput>)> {
    super::scale::set_option(opts.scale);
    let scale = Metrics::of_main_screen(mtm).xpp;
    // `screens` lists the primary (menu bar, origin (0,0)) first.
    let displays: Vec<NativeRect> = NSScreen::screens(mtm)
        .iter()
        .map(|screen| {
            let frame = screen.frame();
            NativeRect {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            }
        })
        .collect();
    let space = GlobalSpace::new(&displays, scale, MAX_ROOT_SIDE);
    log::info!(
        "macos: rootless over {} display(s), root {}x{}, {scale} X px per point",
        displays.len(),
        space.root().0,
        space.root().1
    );
    let layers = Arc::new(LayerTable::default());
    let clock = Arc::new(FrameClock::new(Arc::clone(&link)));
    let pacer = Arc::new(FramePacer::new(Arc::clone(&link)));
    {
        let pacer = Arc::clone(&pacer);
        clock.set_tick_hook(move || pacer.tick());
    }
    let ticker = TickSource::start(&clock)?;
    let input = ShellInput::new(Arc::clone(&link), space.root(), opts.live);
    shell::install(
        space,
        Arc::clone(&link),
        Arc::clone(&layers),
        Rc::clone(&input),
    );
    let spec = RootlessSpec {
        surface_extension: ash::ext::metal_surface::NAME,
        root: space.root(),
        clock,
        link,
        ui: Arc::new(RootlessUi { quit }),
        clients,
    };
    let host = MacRootlessHost {
        layers,
        pacer,
        surfaced: HashSet::new(),
    };
    thread::Builder::new()
        .name("yserver-core".into())
        .spawn(move || {
            server::run_rootless(
                opts,
                spec,
                Box::new(host),
                HostClipboard {
                    pasteboard: Box::new(super::pasteboard::GeneralPasteboard),
                    sync: sync_clipboard,
                },
            )
        })?;
    Ok((ticker, input))
}
