//! The AppKit application: a Quit menu, one window, and the core loop on its
//! own thread. The main thread never touches Vulkan.
//!
//! Shutdown: Quit, the close button and SIGTERM all end in the core loop
//! returning; the core thread then drops every Vulkan object and asks the
//! main thread to quit (`ShellUi::quit`), which answers a pending
//! `applicationShouldTerminate:` or terminates.

use std::{
    cell::{Cell, OnceCell},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use dispatch2::DispatchQueue;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSMenu, NSMenuItem, NSWindow, NSWindowDelegate,
    NSWindowOcclusionState,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString, ns_string};
use objc2_quartz_core::CAMetalLayer;

use super::display_link::DisplayLink;
use crate::{
    launch::LaunchOptions,
    surface::{
        clock::{FrameClock, SoftwareTicker},
        link::{ShellEvent, ShellLink},
        server::{self, ShellUi, WindowSpec},
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
    window: OnceCell<Retained<NSWindow>>,
    /// Retained until exit: the core thread's surface points at it.
    layer: OnceCell<Retained<CAMetalLayer>>,
    /// Ticks the surface's frame clock.
    ticker: OnceCell<TickSource>,
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
        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn occlusion_changed(&self, _notification: &NSNotification) {
            if let Some(window) = self.ivars().window.get() {
                let visible = window
                    .occlusionState()
                    .contains(NSWindowOcclusionState::Visible);
                self.ivars().link.post(ShellEvent::Visible(visible));
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
            window: OnceCell::new(),
            layer: OnceCell::new(),
            ticker: OnceCell::new(),
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
        let (window, layer) = super::window::create(mtm, size);
        let surface = super::window::metal_surface(&layer);
        let _ = ivars.layer.set(layer);
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let window = ivars.window.get_or_init(|| window);
        window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();

        let clock = Arc::new(FrameClock::new(Arc::clone(&ivars.link)));
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
            ui: Box::new(MainQueueUi {
                quit: Arc::clone(&ivars.quit),
            }),
        };
        let spawned = thread::Builder::new()
            .name("yserver-core".into())
            .spawn(move || server::run_windowed(opts, spec));
        if let Err(e) = spawned {
            log::error!("yserver: cannot start the core thread: {e}");
            std::process::exit(1);
        }
    }
}

/// `ShellUi` for the core thread: every command runs on the main queue.
struct MainQueueUi {
    quit: Arc<QuitState>,
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
}

/// A main menu with Quit (Cmd-Q).
fn install_menu(app: &NSApplication, mtm: MainThreadMarker) {
    let quit = {
        // SAFETY: `terminate:` is an `NSApplication` action taking a sender.
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Quit yserver"),
                Some(sel!(terminate:)),
                ns_string!("q"),
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
