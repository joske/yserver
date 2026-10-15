//! The AppKit application: a Quit menu, one window, and the core loop on its
//! own thread. The main thread never touches Vulkan.
//!
//! Every key goes to X, Cmd-Q included (as Super+Q); the menu's Quit has no
//! key equivalent.
//!
//! Shutdown: Quit, the close button and SIGTERM all end in the core loop
//! returning; the core thread then drops every Vulkan object and asks the
//! main thread to quit (`ShellUi::quit`), which answers a pending
//! `applicationShouldTerminate:` or terminates. Quit and the close button
//! ask first while X clients are connected; SIGTERM never asks.

use std::{
    cell::{Cell, OnceCell},
    io,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use dispatch2::DispatchQueue;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSMenu, NSMenuItem, NSWindow,
    NSWindowDelegate, NSWindowOcclusionState,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString, ns_string};
use objc2_quartz_core::CAMetalLayer;

use super::{display_link::DisplayLink, input::ShellInput, scale::Metrics};
use crate::{
    launch::LaunchOptions,
    surface::{
        clock::{FrameClock, SoftwareTicker},
        link::{ShellEvent, ShellLink},
        server::{self, WindowSpec},
        ui::{CursorImage, ShellUi},
    },
};

/// What the main thread and the core thread share about shutdown.
#[derive(Default)]
struct QuitState {
    /// The core loop ended and released Vulkan.
    core_done: AtomicBool,
    /// `applicationShouldTerminate:` answered `TerminateLater`.
    terminate_later: AtomicBool,
}

struct Ivars {
    /// Taken when the application finished launching.
    opts: Cell<Option<LaunchOptions>>,
    link: Arc<ShellLink>,
    quit: Arc<QuitState>,
    /// Established X clients, kept by the core.
    clients: Arc<AtomicUsize>,
    /// The token of the core's last window resize request (0: none), for
    /// the resize event that answers it.
    resize_request: Arc<AtomicU64>,
    /// The user already confirmed quitting (close button, then the
    /// terminate it triggers).
    quit_confirmed: Cell<bool>,
    window: OnceCell<Retained<NSWindow>>,
    /// Retained until exit: the core thread's surface points at it.
    layer: OnceCell<Retained<CAMetalLayer>>,
    /// Ticks the surface's frame clock.
    ticker: OnceCell<TickSource>,
    /// The window's input; the key monitor holds it too.
    input: OnceCell<Rc<ShellInput>>,
    /// The local key monitor, kept for the app's lifetime.
    key_monitor: OnceCell<Retained<AnyObject>>,
}

/// What ticks the frame clock: the display link, or a timer without one.
#[allow(dead_code)] // Held for its `Drop`.
enum TickSource {
    Display(DisplayLink),
    Software(SoftwareTicker),
}

impl TickSource {
    fn start(clock: &Arc<FrameClock>) -> io::Result<Self> {
        match DisplayLink::start(Arc::clone(clock)) {
            Ok(link) => Ok(Self::Display(link)),
            Err(e) => {
                log::warn!("macos: no display link ({e}); ticking at a fixed rate");
                SoftwareTicker::start(Arc::clone(clock)).map(Self::Software)
            }
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no
    // `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct YserverAppDelegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for YserverAppDelegate {}

    // SAFETY: `NSApplicationDelegate` has no safety requirements.
    unsafe impl NSApplicationDelegate for YserverAppDelegate {
        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.open_window();
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            let ivars = self.ivars();
            if ivars.quit.core_done.load(Ordering::Acquire) {
                return NSApplicationTerminateReply::TerminateNow;
            }
            if !self.confirm_quit() {
                return NSApplicationTerminateReply::TerminateCancel;
            }
            ivars.quit.terminate_later.store(true, Ordering::Release);
            ivars.link.request_shutdown();
            NSApplicationTerminateReply::TerminateLater
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            true
        }
    }

    // SAFETY: `NSWindowDelegate` has no safety requirements.
    unsafe impl NSWindowDelegate for YserverAppDelegate {
        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.confirm_quit()
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn occlusion_changed(&self, _notification: &NSNotification) {
            if let Some(window) = self.ivars().window.get() {
                let visible = window
                    .occlusionState()
                    .contains(NSWindowOcclusionState::Visible);
                self.ivars().link.post(ShellEvent::Visible(visible));
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowDidResize:))]
        fn did_resize(&self, _notification: &NSNotification) {
            // The view's `setFrameSize:` reports it too; repeats are dropped.
            if let Some(view) = self.ivars().window.get().and_then(|w| w.contentView()) {
                super::resize::view_resized(&view);
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowDidChangeScreen:))]
        fn did_change_screen(&self, _notification: &NSNotification) {
            // Another screen may have other millimetres per point at the
            // same backing scale.
            if let Some(view) = self.ivars().window.get().and_then(|w| w.contentView()) {
                super::resize::backing_changed(&view);
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowDidBecomeKey:))]
        fn did_become_key(&self, _notification: &NSNotification) {
            if let Some(input) = self.ivars().input.get() {
                input.focus_gained();
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _notification: &NSNotification) {
            if let Some(input) = self.ivars().input.get() {
                input.focus_lost();
            }
        }
    }
);

impl YserverAppDelegate {
    fn new(mtm: MainThreadMarker, opts: LaunchOptions) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            opts: Cell::new(Some(opts)),
            link: Arc::new(ShellLink::default()),
            quit: Arc::new(QuitState::default()),
            clients: Arc::default(),
            resize_request: Arc::default(),
            quit_confirmed: Cell::new(false),
            window: OnceCell::new(),
            layer: OnceCell::new(),
            ticker: OnceCell::new(),
            input: OnceCell::new(),
            key_monitor: OnceCell::new(),
        });
        // SAFETY: `NSObject`'s `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    /// Show the window and start the core thread on it.
    fn open_window(&self) {
        let mtm = self.mtm();
        let ivars = self.ivars();
        let Some(opts) = ivars.opts.take() else {
            return;
        };
        let size = opts.geometry.unwrap_or(server::DEFAULT_SIZE);
        super::scale::set_option(opts.scale);
        let input = ShellInput::new(Arc::clone(&ivars.link), size);
        match input.install_key_monitor(mtm) {
            Some(monitor) => {
                let _ = ivars.key_monitor.set(monitor);
            }
            None => log::error!("macos: no key monitor; keys reach AppKit, not X"),
        }
        let _ = ivars.input.set(Rc::clone(&input));
        let (window, layer) = super::window::create(mtm, size, Rc::clone(&input));
        let surface = super::window::metal_surface(&layer);
        let clock = Arc::new(FrameClock::new(Arc::clone(&ivars.link)));
        let metrics = Metrics::of_window(&window);
        if let Some(view) = window.contentView() {
            super::resize::install(
                &view,
                size,
                metrics,
                Arc::clone(&ivars.link),
                Arc::clone(&ivars.resize_request),
                layer.clone(),
                input,
                &clock,
            );
        }
        let _ = ivars.layer.set(layer);
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let window = ivars.window.get_or_init(|| window);
        window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();

        match TickSource::start(&clock) {
            Ok(ticker) => {
                let _ = ivars.ticker.set(ticker);
            }
            Err(e) => {
                log::error!("yserver: cannot start the frame clock: {e}");
                std::process::exit(1);
            }
        }
        let spec = WindowSpec {
            surface,
            clock,
            link: Arc::clone(&ivars.link),
            ui: Arc::new(MainQueueUi {
                quit: Arc::clone(&ivars.quit),
                resize_request: Arc::clone(&ivars.resize_request),
            }),
            clients: Arc::clone(&ivars.clients),
            max_output: super::window::max_content_size(mtm),
            output_mm: metrics.mm(size),
            copy_scale: metrics.factor,
            pasteboard: Box::new(super::pasteboard::GeneralPasteboard),
        };
        let spawned = thread::Builder::new()
            .name("yserver-core".into())
            .spawn(move || server::run_windowed(opts, spec));
        if let Err(e) = spawned {
            log::error!("yserver: cannot start the core thread: {e}");
            std::process::exit(1);
        }
    }

    /// Whether to quit: asks while X clients are connected, once per quit.
    fn confirm_quit(&self) -> bool {
        let ivars = self.ivars();
        if ivars.quit_confirmed.get() {
            return true;
        }
        let clients = ivars.clients.load(Ordering::Acquire);
        if clients > 0 && !ask_quit(self.mtm(), clients) {
            return false;
        }
        ivars.quit_confirmed.set(true);
        true
    }
}

/// Ask whether to quit with `clients` X clients connected.
fn ask_quit(mtm: MainThreadMarker, clients: usize) -> bool {
    let alert = NSAlert::new(mtm);
    let noun = if clients == 1 { "client" } else { "clients" };
    alert.setMessageText(&NSString::from_str(&format!(
        "{clients} X {noun} connected"
    )));
    alert.setInformativeText(ns_string!("Quitting yserver disconnects them."));
    alert.addButtonWithTitle(ns_string!("Quit"));
    alert.addButtonWithTitle(ns_string!("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

/// `ShellUi` for the core thread: every command runs on the main queue.
struct MainQueueUi {
    quit: Arc<QuitState>,
    resize_request: Arc<AtomicU64>,
}

impl ShellUi for MainQueueUi {
    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            if let Some(window) = NSApplication::sharedApplication(mtm)
                .windows()
                .firstObject()
            {
                window.setTitle(&NSString::from_str(&title));
            }
        });
    }

    fn quit(&self, failed: bool) {
        self.quit.core_done.store(true, Ordering::Release);
        let quit = Arc::clone(&self.quit);
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            let app = NSApplication::sharedApplication(mtm);
            if quit.terminate_later.load(Ordering::Acquire) {
                app.replyToApplicationShouldTerminate(true);
            } else if failed {
                std::process::exit(1);
            } else {
                app.terminate(None);
            }
        });
    }

    fn set_cursor(&self, cursor: Option<CursorImage>) {
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            super::cursor::set(mtm, cursor);
        });
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            super::cursor::warp(mtm, x, y);
        });
    }

    fn set_content_size(&self, width: u16, height: u16, token: u64) {
        let request = Arc::clone(&self.resize_request);
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            let Some(window) = NSApplication::sharedApplication(mtm)
                .windows()
                .firstObject()
            else {
                return;
            };
            // Before the resize: the view holds back its own report.
            request.store(token, Ordering::Release);
            window.setContentSize(Metrics::of_window(&window).points((width, height)));
            // Synchronous: the window has its final (maybe clamped) size.
            super::resize::answer_request(&window);
        });
    }
}

/// A main menu with Quit, without a key equivalent: Cmd-Q belongs to X.
fn install_menu(app: &NSApplication, mtm: MainThreadMarker) {
    let quit = {
        // SAFETY: `terminate:` is an `NSApplication` action taking a sender.
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Quit yserver"),
                Some(sel!(terminate:)),
                ns_string!(""),
            )
        }
    };
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&quit);
    let app_item = NSMenuItem::new(mtm);
    app_item.setSubmenu(Some(&*app_menu));
    let menu_bar = NSMenu::new(mtm);
    menu_bar.addItem(&app_item);
    app.setMainMenu(Some(&*menu_bar));
}

/// Run the AppKit application on the main thread; does not return on a
/// normal quit (AppKit exits the process).
///
/// # Errors
///
/// Not called on the main thread.
pub(super) fn run(opts: LaunchOptions) -> io::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("the window shell must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    install_menu(&app, mtm);
    let delegate = YserverAppDelegate::new(mtm, opts);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    Ok(())
}
