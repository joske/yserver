//! The AppKit application: the app menu, one window (rooted) or one window
//! per X top-level (rootless, `super::rootless`), and the core loop on its
//! own thread. The main thread never touches Vulkan.
//!
//! Every key goes to X; with key equivalents on, the menu's shortcuts
//! (Cmd-Q, Cmd-H, Cmd-Alt-H, Cmd-,) go to the menu (`super::input`).
//!
//! Shutdown: Quit, the close button and SIGTERM all end in the core loop
//! returning; the core thread then drops every Vulkan object and asks the
//! main thread to quit (`ShellUi::quit`), which answers a pending
//! `applicationShouldTerminate:` or terminates. Quit and the close button
//! ask first while X clients are connected; SIGTERM never asks. Restart
//! (Settings) is a Quit that, once the server is torn down and AppKit
//! terminates, leaves a helper behind to open the bundle again
//! (`crate::app_bundle::relaunch_command`).

use std::{
    cell::{Cell, OnceCell},
    io,
    path::PathBuf,
    process::Stdio,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use dispatch2::DispatchQueue;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSEventModifierFlags, NSImage, NSMenu,
    NSMenuItem, NSWindow, NSWindowDelegate, NSWindowOcclusionState,
};
use objc2_foundation::{NSData, NSNotification, NSObject, NSObjectProtocol, NSString, ns_string};
use objc2_quartz_core::CAMetalLayer;

use super::{
    display_link::DisplayLink,
    input::ShellInput,
    scale::Metrics,
    settings_window::{self, SettingsWindow},
};
use crate::{
    launch::{LaunchOptions, ScaleOption, ShellMode},
    settings::{Mode, SetByArgv},
    surface::{
        clipboard::HostClipboard,
        clock::{FrameClock, SoftwareTicker},
        link::{ShellEvent, ShellLink},
        server::{self, WindowSpec},
        ui::{CursorImage, ShellUi},
    },
};

/// What the main thread and the core thread share about shutdown.
#[derive(Default)]
pub(super) struct QuitState {
    /// The core loop ended and released Vulkan.
    core_done: AtomicBool,
    /// `applicationShouldTerminate:` answered `TerminateLater`.
    terminate_later: AtomicBool,
}

struct Ivars {
    /// Taken when the application finished launching.
    opts: Cell<Option<LaunchOptions>>,
    mode: ShellMode,
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
    /// Releases held keys when a non-X window becomes key.
    focus_observer: OnceCell<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    /// The clipboard bridge's switch, the live `sync_clipboard`.
    sync_clipboard: Arc<AtomicBool>,
    /// What argv fixed for this run, and the scale it runs at.
    set_by_argv: SetByArgv,
    scale: ScaleOption,
    /// Made on first open.
    settings: OnceCell<SettingsWindow>,
    /// The bundle this run started from, which a restart opens again.
    bundle: Option<PathBuf>,
    /// Argv changed this run.
    argv_overrides: bool,
    /// Restart asked for: relaunch on termination.
    restart: Rc<Cell<bool>>,
}

/// What ticks the frame clock: the display link, or a timer without one.
#[allow(dead_code)] // Held for its `Drop`.
pub(super) enum TickSource {
    Display(DisplayLink),
    Software(SoftwareTicker),
}

impl TickSource {
    pub(super) fn start(clock: &Arc<FrameClock>) -> io::Result<Self> {
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
            match self.ivars().mode {
                ShellMode::Rooted => self.open_window(),
                ShellMode::Rootless => self.start_rootless(),
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            let ivars = self.ivars();
            if ivars.quit.core_done.load(Ordering::Acquire) {
                return NSApplicationTerminateReply::TerminateNow;
            }
            if !self.confirm_quit() {
                ivars.restart.set(false);
                return NSApplicationTerminateReply::TerminateCancel;
            }
            ivars.quit.terminate_later.store(true, Ordering::Release);
            ivars.link.request_shutdown();
            NSApplicationTerminateReply::TerminateLater
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            // The core loop has ended: clients, socket and lock are gone.
            let ivars = self.ivars();
            if ivars.restart.get()
                && let Some(bundle) = &ivars.bundle
            {
                relaunch(bundle);
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationDidBecomeActive:))]
        fn did_become_active(&self, _notification: &NSNotification) {
            if self.ivars().mode == ShellMode::Rootless {
                super::rootless::app_activated();
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationDidResignActive:))]
        fn did_resign_active(&self, _notification: &NSNotification) {
            // Rooted: the window's resign-key releases the keys.
            if self.ivars().mode == ShellMode::Rootless {
                super::rootless::app_deactivated();
            }
        }

        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window_closed(&self, _sender: &NSApplication) -> bool {
            // Rootless: closing the last X window does not quit.
            self.ivars().mode == ShellMode::Rooted
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

    impl YserverAppDelegate {
        // SAFETY: an action method: one object argument, no return value.
        #[unsafe(method(copyDisplay:))]
        fn copy_display(&self, _sender: Option<&AnyObject>) {
            super::display::copy();
        }

        // SAFETY: an action method: one object argument, no return value.
        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            let (mtm, ivars) = (self.mtm(), self.ivars());
            let Some(input) = ivars.input.get() else {
                return;
            };
            let window = ivars.settings.get_or_init(|| {
                SettingsWindow::new(
                    mtm,
                    settings_window::Context {
                        input: Rc::clone(input),
                        sync_clipboard: Arc::clone(&ivars.sync_clipboard),
                        set_by_argv: ivars.set_by_argv,
                        mode: match ivars.mode {
                            ShellMode::Rooted => Mode::Window,
                            ShellMode::Rootless => Mode::Rootless,
                        },
                        scale: ivars.scale,
                        from_bundle: ivars.bundle.is_some(),
                        argv_overrides: ivars.argv_overrides,
                        restart: Rc::clone(&ivars.restart),
                    },
                )
            });
            window.show(mtm);
        }
    }
);

impl YserverAppDelegate {
    fn new(mtm: MainThreadMarker, opts: LaunchOptions, mode: ShellMode) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            sync_clipboard: Arc::new(AtomicBool::new(opts.live.sync_clipboard)),
            set_by_argv: opts.set_by_argv,
            scale: opts.scale,
            settings: OnceCell::new(),
            bundle: opts
                .from_bundle
                .then(|| std::env::current_exe().ok())
                .flatten()
                .and_then(|exe| crate::app_bundle::bundle_of(&exe).map(PathBuf::from)),
            argv_overrides: opts.argv_overrides,
            restart: Rc::new(Cell::new(false)),
            opts: Cell::new(Some(opts)),
            mode,
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
            focus_observer: OnceCell::new(),
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
        let input = ShellInput::new(Arc::clone(&ivars.link), size, opts.live);
        self.attach_input(Rc::clone(&input));
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
            clipboard: HostClipboard {
                pasteboard: Box::new(super::pasteboard::GeneralPasteboard),
                sync: Arc::clone(&ivars.sync_clipboard),
            },
        };
        let spawned = thread::Builder::new()
            .name("yserver-core".into())
            .spawn(move || server::run_windowed(opts, spec));
        if let Err(e) = spawned {
            log::error!("yserver: cannot start the core thread: {e}");
            std::process::exit(1);
        }
    }

    /// Start rootless mode: no root window, one window per top-level.
    fn start_rootless(&self) {
        let mtm = self.mtm();
        let ivars = self.ivars();
        let Some(opts) = ivars.opts.take() else {
            return;
        };
        NSApplication::sharedApplication(mtm).activate();
        match super::rootless::start(
            mtm,
            opts,
            Arc::clone(&ivars.link),
            Arc::clone(&ivars.quit),
            Arc::clone(&ivars.clients),
            Arc::clone(&ivars.sync_clipboard),
        ) {
            Ok((ticker, input)) => {
                let _ = ivars.ticker.set(ticker);
                self.attach_input(input);
            }
            Err(e) => {
                log::error!("yserver: cannot start rootless mode: {e}");
                std::process::exit(1);
            }
        }
    }

    /// Take the app's keys and focus changes for `input`.
    fn attach_input(&self, input: Rc<ShellInput>) {
        let ivars = self.ivars();
        match input.install_key_monitor(self.mtm()) {
            Some(monitor) => {
                let _ = ivars.key_monitor.set(monitor);
            }
            None => log::error!("macos: no key monitor; keys reach AppKit, not X"),
        }
        let _ = ivars.focus_observer.set(input.install_focus_observer());
        let _ = ivars.input.set(input);
    }

    /// Whether to quit: asks while X clients are connected, once per quit.
    fn confirm_quit(&self) -> bool {
        let ivars = self.ivars();
        if ivars.quit_confirmed.get() {
            return true;
        }
        let clients = ivars.clients.load(Ordering::Acquire);
        if clients > 0 && !ask_quit(self.mtm(), clients, ivars.restart.get()) {
            return false;
        }
        ivars.quit_confirmed.set(true);
        true
    }
}

/// Ask whether to quit, or to `restart`, with `clients` X clients
/// connected.
fn ask_quit(mtm: MainThreadMarker, clients: usize, restart: bool) -> bool {
    let alert = NSAlert::new(mtm);
    let noun = if clients == 1 { "client" } else { "clients" };
    alert.setMessageText(&NSString::from_str(&format!(
        "{clients} X {noun} connected"
    )));
    if restart {
        alert.setInformativeText(ns_string!("Restarting yserver disconnects them."));
        alert.addButtonWithTitle(ns_string!("Restart"));
    } else {
        alert.setInformativeText(ns_string!("Quitting yserver disconnects them."));
        alert.addButtonWithTitle(ns_string!("Quit"));
    }
    alert.addButtonWithTitle(ns_string!("Cancel"));
    alert.runModal() == NSAlertFirstButtonReturn
}

/// `ShellUi` for the core thread: every command runs on the main queue.
struct MainQueueUi {
    quit: Arc<QuitState>,
    resize_request: Arc<AtomicU64>,
}

impl ShellUi for MainQueueUi {
    fn set_display(&self, display: u16) {
        DispatchQueue::main().exec_async(move || {
            let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
            super::display::announce(mtm, display);
            if let Some(window) = super::window::root_window(mtm) {
                window.setTitle(&NSString::from_str(&super::display::title(display)));
            }
        });
    }

    fn quit(&self, failed: bool) {
        quit_app(&self.quit, failed);
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
            let Some(window) = super::window::root_window(mtm) else {
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

/// The core loop ended and released Vulkan: answer a pending terminate, or
/// quit (exit 1 when it `failed`).
pub(super) fn quit_app(quit: &Arc<QuitState>, failed: bool) {
    quit.core_done.store(true, Ordering::Release);
    let quit = Arc::clone(quit);
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

/// Leave a helper that opens `bundle` again once this process has exited.
fn relaunch(bundle: &std::path::Path) {
    use std::os::unix::process::CommandExt;
    let spawned = crate::app_bundle::relaunch_command(std::process::id(), bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match spawned {
        Ok(child) => log::info!(
            "macos: restart: relaunching {} (helper {})",
            bundle.display(),
            child.id()
        ),
        Err(e) => log::error!("macos: restart: cannot relaunch: {e}"),
    }
}

/// A nil-targeted menu item: the action goes up the responder chain, to
/// `NSApplication` or the app delegate.
fn menu_item(mtm: MainThreadMarker, title: &str, action: Sel, key: &str) -> Retained<NSMenuItem> {
    // SAFETY: every action passed is an `NSApplication` or delegate action
    // taking a sender.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    }
}

/// The app menu. Its shortcuts reach it only with key equivalents on; else
/// the key monitor gives them to X.
fn install_menu(app: &NSApplication, mtm: MainThreadMarker) {
    let hide_others = menu_item(mtm, "Hide Others", sel!(hideOtherApplications:), "h");
    hide_others
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    let app_menu = NSMenu::new(mtm);
    for item in [
        menu_item(
            mtm,
            "About yserver",
            sel!(orderFrontStandardAboutPanel:),
            "",
        ),
        menu_item(mtm, "Settings…", sel!(showSettings:), ","),
        NSMenuItem::separatorItem(mtm),
        menu_item(mtm, "Hide yserver", sel!(hide:), "h"),
        hide_others,
        menu_item(mtm, "Show All", sel!(unhideAllApplications:), ""),
        NSMenuItem::separatorItem(mtm),
        menu_item(mtm, "Quit yserver", sel!(terminate:), "q"),
    ] {
        app_menu.addItem(&item);
    }
    let app_item = NSMenuItem::new(mtm);
    app_item.setSubmenu(Some(&*app_menu));
    let menu_bar = NSMenu::new(mtm);
    menu_bar.addItem(&app_item);
    app.setMainMenu(Some(&*menu_bar));
}

/// The Dock icon, set at run time so a bare binary started from a terminal
/// gets it too, not only the bundle.
fn set_dock_icon(app: &NSApplication) {
    const ICON_PNG: &[u8] = include_bytes!("../../../../assets/yserver-icon.png");
    let data = NSData::with_bytes(ICON_PNG);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        // SAFETY: a non-nil image is always a valid application icon.
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

/// Run the AppKit application on the main thread; does not return on a
/// normal quit (AppKit exits the process).
///
/// # Errors
///
/// Not called on the main thread.
pub(super) fn run(opts: LaunchOptions, mode: ShellMode) -> io::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("the window shell must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    set_dock_icon(&app);
    install_menu(&app, mtm);
    let delegate = YserverAppDelegate::new(mtm, opts, mode);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    Ok(())
}
