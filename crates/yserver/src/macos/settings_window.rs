//! The Settings window (app menu → Settings…, Cmd-,), built in code: one
//! instance, brought to front on reopen. Every change is written to the user
//! defaults at once; the live settings also apply at once. A setting a
//! command-line flag fixed for this run is shown disabled and never written.
//! Launch at login is `SMAppService.mainApp`'s state, never stored; Restart
//! quits as Quit does and relaunches the bundle (`super::app`).

use std::{
    cell::{Cell, OnceCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSAlert, NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSLayoutAttribute, NSPopUpButton, NSStackView, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSEdgeInsets, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_service_management::SMAppService;

use super::{defaults::UserDefaults, input::ShellInput};
use crate::{
    launch::ScaleOption,
    settings::{
        KEY_EMULATE_BUTTONS, KEY_KEY_EQUIVALENTS, KEY_MODE, KEY_SCALE, KEY_SYNC_CLIPBOARD, Live,
        LoginItem, Mode, SetByArgv, Store, restart_view, scale_str,
    },
};

/// The scale popup's items, in order.
const SCALES: [(ScaleOption, &str); 3] = [
    (ScaleOption::Auto, "Automatic"),
    (ScaleOption::Factor(1), "1 (one X pixel per point)"),
    (ScaleOption::Factor(2), "2 (one X pixel per Retina pixel)"),
];

/// What the window shows and changes besides the stored settings.
pub(super) struct Context {
    pub(super) input: Rc<ShellInput>,
    /// The clipboard bridge's switch.
    pub(super) sync_clipboard: Arc<AtomicBool>,
    pub(super) set_by_argv: SetByArgv,
    /// This run's mode and scale, shown for what argv fixed.
    pub(super) mode: Mode,
    pub(super) scale: ScaleOption,
    /// Running from the app bundle: launch at login and Restart need it.
    pub(super) from_bundle: bool,
    /// Argv changed this run; a restart drops it.
    pub(super) argv_overrides: bool,
    /// Set by Restart before it asks the app to quit; the app delegate
    /// relaunches on termination, and clears it when the quit is cancelled.
    pub(super) restart: Rc<Cell<bool>>,
}

/// What the target updates when the launch-at-login state is re-read.
struct LoginRow {
    checkbox: Retained<NSButton>,
    note: Retained<NSTextField>,
    open: Retained<NSButton>,
}

struct TargetIvars {
    context: Context,
    login: OnceCell<LoginRow>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no
    // `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    struct SettingsTarget;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for SettingsTarget {}

    // SAFETY: `NSWindowDelegate` has no safety requirements.
    unsafe impl NSWindowDelegate for SettingsTarget {
        // SAFETY: the signature matches the protocol method.
        #[unsafe(method(windowDidBecomeKey:))]
        fn did_become_key(&self, _notification: &NSNotification) {
            // System Settings may have changed it meanwhile.
            self.refresh_login();
        }
    }

    impl SettingsTarget {
        // SAFETY (all actions): one object argument, the sending control;
        // no return value.
        #[unsafe(method(setMode:))]
        fn set_mode(&self, sender: &NSButton) {
            let mode = if sender.tag() == 1 {
                Mode::Window
            } else {
                Mode::Rootless
            };
            UserDefaults::standard().set_string(KEY_MODE, mode.as_str());
        }

        #[unsafe(method(setScale:))]
        fn set_scale(&self, sender: &NSPopUpButton) {
            let at = usize::try_from(sender.indexOfSelectedItem()).unwrap_or(0);
            let scale = SCALES.get(at).map_or(ScaleOption::Auto, |&(scale, _)| scale);
            UserDefaults::standard().set_string(KEY_SCALE, &scale_str(scale));
        }

        #[unsafe(method(setEmulateButtons:))]
        fn set_emulate_buttons(&self, sender: &NSButton) {
            self.set_live(KEY_EMULATE_BUTTONS, sender, |live, on| {
                live.emulate_buttons = on;
            });
        }

        #[unsafe(method(setKeyEquivalents:))]
        fn set_key_equivalents(&self, sender: &NSButton) {
            self.set_live(KEY_KEY_EQUIVALENTS, sender, |live, on| {
                live.key_equivalents = on;
            });
        }

        #[unsafe(method(setSyncClipboard:))]
        fn set_sync_clipboard(&self, sender: &NSButton) {
            self.set_live(KEY_SYNC_CLIPBOARD, sender, |live, on| {
                live.sync_clipboard = on;
            });
        }

        #[unsafe(method(setLaunchAtLogin:))]
        fn set_launch_at_login(&self, sender: &NSButton) {
            let on = sender.state() == NSControlStateValueOn;
            let result = set_login_item(on);
            log::info!("macos: settings: launch at login = {on}: {result:?}");
            self.refresh_login();
            if let Err(e) = result {
                let alert = NSAlert::new(self.mtm());
                alert.setMessageText(&NSString::from_str(if on {
                    "yserver could not be added to the login items"
                } else {
                    "yserver could not be removed from the login items"
                }));
                alert.setInformativeText(&NSString::from_str(&e));
                alert.runModal();
            }
        }

        #[unsafe(method(openLoginItems:))]
        fn open_login_items(&self, _sender: &NSButton) {
            // SAFETY: a class method without arguments.
            unsafe { SMAppService::openSystemSettingsLoginItems() };
        }

        #[unsafe(method(copyDisplay:))]
        fn copy_display(&self, _sender: &NSButton) {
            super::display::copy();
        }

        #[unsafe(method(restart:))]
        fn restart(&self, _sender: &NSButton) {
            log::info!("macos: settings: restart");
            self.ivars().context.restart.set(true);
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl SettingsTarget {
    fn new(mtm: MainThreadMarker, context: Context) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            context,
            login: OnceCell::new(),
        });
        // SAFETY: `NSObject`'s `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    /// Store a live checkbox's state under `key` and apply it now.
    fn set_live(&self, key: &str, sender: &NSButton, change: impl FnOnce(&mut Live, bool)) {
        let on = sender.state() == NSControlStateValueOn;
        UserDefaults::standard().set_bool(key, on);
        let context = &self.ivars().context;
        let mut live = context.input.live();
        change(&mut live, on);
        context.input.set_live(live);
        context
            .sync_clipboard
            .store(live.sync_clipboard, Ordering::Release);
        log::info!("macos: settings: {key} = {on}");
    }

    /// Show the launch-at-login state as it is now.
    fn refresh_login(&self) {
        let ivars = self.ivars();
        let Some(row) = ivars.login.get() else {
            return;
        };
        let view = login_item(ivars.context.from_bundle).view();
        row.checkbox.setState(if view.checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        row.checkbox.setEnabled(view.enabled);
        row.note
            .setStringValue(&NSString::from_str(view.note.unwrap_or("")));
        row.note.setHidden(view.note.is_none());
        row.open.setHidden(!view.open_login_items);
    }
}

/// `SMAppService.mainApp`'s state; it needs the bundle.
fn login_item(from_bundle: bool) -> LoginItem {
    if !from_bundle {
        return LoginItem::NoBundle;
    }
    // SAFETY: class method and getter without arguments.
    let status = unsafe { SMAppService::mainAppService().status() };
    LoginItem::from_status(status.0)
}

/// Register or unregister the app as a login item; the error's
/// description on failure.
fn set_login_item(on: bool) -> Result<(), String> {
    // SAFETY: class method without arguments.
    let service = unsafe { SMAppService::mainAppService() };
    // SAFETY: methods without arguments besides the error out-parameter.
    let result = unsafe {
        if on {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    result.map_err(|e| e.localizedDescription().to_string())
}

/// The window and the target its controls send to (controls do not
/// retain their target).
pub(super) struct SettingsWindow {
    window: Retained<NSWindow>,
    display: Retained<NSTextField>,
    _target: Retained<SettingsTarget>,
}

impl SettingsWindow {
    pub(super) fn new(mtm: MainThreadMarker, context: Context) -> Self {
        let stored = super::defaults::stored_settings();
        let live = context.input.live();
        let set_by_argv = context.set_by_argv;
        let (from_bundle, argv_overrides) = (context.from_bundle, context.argv_overrides);
        let (mode, scale) = (
            if set_by_argv.mode {
                context.mode
            } else {
                stored.mode
            },
            if set_by_argv.scale {
                context.scale
            } else {
                stored.scale
            },
        );
        let target = SettingsTarget::new(mtm, context);
        let target_obj: &AnyObject = &target;

        let rootless = radio(mtm, "Rootless (a Mac window per X window)", 0, target_obj);
        let window_mode = radio(mtm, "One window for the X screen", 1, target_obj);
        let selected = if mode == Mode::Window {
            &window_mode
        } else {
            &rootless
        };
        selected.setState(NSControlStateValueOn);
        let mode_row = column(mtm, &[&rootless, &window_mode], 4.0);
        for radio in [&rootless, &window_mode] {
            radio.setEnabled(!set_by_argv.mode);
        }

        let popup =
            NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::ZERO, false);
        for (_, title) in SCALES {
            popup.addItemWithTitle(&NSString::from_str(title));
        }
        let at = SCALES.iter().position(|&(s, _)| s == scale).unwrap_or(0);
        popup.selectItemAtIndex(at as isize);
        // SAFETY: `setScale:` is the target's action taking a sender.
        unsafe {
            popup.setTarget(Some(target_obj));
            popup.setAction(Some(sel!(setScale:)));
        }
        popup.setEnabled(!set_by_argv.scale);

        let display_label = label(mtm, &display_text());
        let copy_display = button(mtm, "Copy", Some((sel!(copyDisplay:), target_obj)));
        let display_line = row(
            mtm,
            &[&label(mtm, "X display:"), &display_label, &copy_display],
        );

        let mode_line = row(mtm, &[&label(mtm, "Mode:"), &mode_row]);
        let scale_line = row(mtm, &[&label(mtm, "Scale:"), &popup]);
        for (line, fixed) in [
            (&mode_line, set_by_argv.mode),
            (&scale_line, set_by_argv.scale),
        ] {
            if fixed {
                line.addArrangedSubview(&note(mtm, "set by command line"));
            }
        }

        let (can_restart, restart_note) = restart_view(from_bundle, argv_overrides);
        let restart = button(mtm, "Restart yserver", Some((sel!(restart:), target_obj)));
        restart.setEnabled(can_restart);
        let restart_line = row(mtm, &[&note(mtm, restart_note), &restart]);

        let emulate = checkbox(
            mtm,
            "Option-click = middle, Command-click = right",
            live.emulate_buttons,
            Some((sel!(setEmulateButtons:), target_obj)),
        );
        let key_equivalents = checkbox(
            mtm,
            "Command shortcuts go to yserver (Cmd-Q, Cmd-H, Cmd-,)",
            live.key_equivalents,
            Some((sel!(setKeyEquivalents:), target_obj)),
        );
        let clipboard = checkbox(
            mtm,
            "Sync the clipboard with the Mac pasteboard",
            live.sync_clipboard,
            Some((sel!(setSyncClipboard:), target_obj)),
        );
        let login = checkbox(
            mtm,
            "Open yserver at login",
            false,
            Some((sel!(setLaunchAtLogin:), target_obj)),
        );
        let login_note = note(mtm, "");
        let open_login_items = button(
            mtm,
            "Open Login Items…",
            Some((sel!(openLoginItems:), target_obj)),
        );
        let login_hint = row(mtm, &[&login_note, &open_login_items]);
        let login_line = column(mtm, &[&login, &login_hint], 4.0);
        let _ = target.ivars().login.set(LoginRow {
            checkbox: login,
            note: login_note,
            open: open_login_items,
        });
        target.refresh_login();

        let content = column(
            mtm,
            &[
                &display_line,
                &mode_line,
                &scale_line,
                &restart_line,
                &emulate,
                &key_equivalents,
                &clipboard,
                &login_line,
            ],
            12.0,
        );
        content.setEdgeInsets(NSEdgeInsets {
            top: 20.0,
            left: 20.0,
            bottom: 20.0,
            right: 20.0,
        });
        content.setCustomSpacing_afterView(20.0, &restart_line);
        content.setCustomSpacing_afterView(20.0, &display_line);

        // SAFETY: a fresh allocation; released-when-closed is turned off below.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 320.0)),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the app delegate keeps the window outside a controller.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("yserver Settings"));
        window.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        window.setContentView(Some(&content));
        content.layoutSubtreeIfNeeded();
        window.setContentSize(content.fittingSize());
        window.center();
        Self {
            window,
            display: display_label,
            _target: target,
        }
    }

    /// Bring the window to front and make it key.
    pub(super) fn show(&self, mtm: MainThreadMarker) {
        self.display
            .setStringValue(&NSString::from_str(&display_text()));
        NSApplication::sharedApplication(mtm).activate();
        self.window.makeKeyAndOrderFront(None);
    }
}

/// The display's name, or that it is not known yet.
fn display_text() -> String {
    super::display::name().unwrap_or_else(|| "starting".to_owned())
}

/// A radio button for `setMode:`; `tag` 1 is the window mode. Radios with
/// one superview and one action are exclusive.
fn radio(mtm: MainThreadMarker, title: &str, tag: isize, target: &AnyObject) -> Retained<NSButton> {
    // SAFETY: `setMode:` is the target's action taking a sender.
    let radio = unsafe {
        NSButton::radioButtonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(sel!(setMode:)),
            mtm,
        )
    };
    radio.setTag(tag);
    radio
}

/// A checkbox sending `action` to its target.
fn checkbox(
    mtm: MainThreadMarker,
    title: &str,
    on: bool,
    action: Option<(Sel, &AnyObject)>,
) -> Retained<NSButton> {
    // SAFETY: every action passed is its target's, taking a sender.
    let checkbox = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            action.map(|(_, target)| target),
            action.map(|(action, _)| action),
            mtm,
        )
    };
    checkbox.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    checkbox
}

/// A push button sending `action` to its target.
fn button(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<(Sel, &AnyObject)>,
) -> Retained<NSButton> {
    // SAFETY: every action passed is its target's, taking a sender.
    unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            action.map(|(_, target)| target),
            action.map(|(action, _)| action),
            mtm,
        )
    }
}

fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

/// A small secondary label.
fn note(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let note = label(mtm, text);
    note.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    note.setTextColor(Some(&NSColor::secondaryLabelColor()));
    note
}

fn stack(
    mtm: MainThreadMarker,
    views: &[&NSView],
    orientation: NSUserInterfaceLayoutOrientation,
) -> Retained<NSStackView> {
    let stack = NSStackView::new(mtm);
    stack.setOrientation(orientation);
    for view in views {
        stack.addArrangedSubview(view);
    }
    stack
}

/// Views side by side.
fn row(mtm: MainThreadMarker, views: &[&NSView]) -> Retained<NSStackView> {
    let row = stack(mtm, views, NSUserInterfaceLayoutOrientation::Horizontal);
    row.setAlignment(NSLayoutAttribute::FirstBaseline);
    row
}

/// Views stacked, leading edges aligned, `spacing` apart.
fn column(mtm: MainThreadMarker, views: &[&NSView], spacing: f64) -> Retained<NSStackView> {
    let column = stack(mtm, views, NSUserInterfaceLayoutOrientation::Vertical);
    column.setAlignment(NSLayoutAttribute::Leading);
    column.setSpacing(spacing);
    column
}
