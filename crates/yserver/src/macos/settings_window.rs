//! The Settings window (app menu → Settings…, Cmd-,), built in code: one
//! instance, brought to front on reopen. Every change is written to the user
//! defaults at once; the live settings also apply at once. A setting a
//! command-line flag fixed for this run is shown disabled and never written.

use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSLayoutAttribute, NSPopUpButton, NSStackView, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSEdgeInsets, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::{defaults::UserDefaults, input::ShellInput};
use crate::{
    launch::ScaleOption,
    settings::{
        KEY_EMULATE_BUTTONS, KEY_KEY_EQUIVALENTS, KEY_MODE, KEY_SCALE, KEY_SYNC_CLIPBOARD, Live,
        Mode, SetByArgv, Store, scale_str,
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
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class has no
    // `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Context]
    struct SettingsTarget;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for SettingsTarget {}

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
    }
);

impl SettingsTarget {
    fn new(mtm: MainThreadMarker, context: Context) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(context);
        // SAFETY: `NSObject`'s `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    /// Store a live checkbox's state under `key` and apply it now.
    fn set_live(&self, key: &str, sender: &NSButton, change: impl FnOnce(&mut Live, bool)) {
        let on = sender.state() == NSControlStateValueOn;
        UserDefaults::standard().set_bool(key, on);
        let context = self.ivars();
        let mut live = context.input.live();
        change(&mut live, on);
        context.input.set_live(live);
        context
            .sync_clipboard
            .store(live.sync_clipboard, Ordering::Release);
        log::info!("macos: settings: {key} = {on}");
    }
}

/// The window and the target its controls send to (controls do not
/// retain their target).
pub(super) struct SettingsWindow {
    window: Retained<NSWindow>,
    _target: Retained<SettingsTarget>,
}

impl SettingsWindow {
    pub(super) fn new(mtm: MainThreadMarker, context: Context) -> Self {
        let stored = super::defaults::stored_settings();
        let live = context.input.live();
        let set_by_argv = context.set_by_argv;
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

        let restart = button(mtm, "Restart yserver");
        restart.setEnabled(false);
        let restart_line = row(
            mtm,
            &[&note(mtm, "Takes effect when yserver restarts"), &restart],
        );

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
        let login = checkbox(mtm, "Open yserver at login", false, None);
        login.setEnabled(false);

        let content = column(
            mtm,
            &[
                &mode_line,
                &scale_line,
                &restart_line,
                &emulate,
                &key_equivalents,
                &clipboard,
                &login,
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
        window.setContentView(Some(&content));
        content.layoutSubtreeIfNeeded();
        window.setContentSize(content.fittingSize());
        window.center();
        Self {
            window,
            _target: target,
        }
    }

    /// Bring the window to front and make it key.
    pub(super) fn show(&self, mtm: MainThreadMarker) {
        NSApplication::sharedApplication(mtm).activate();
        self.window.makeKeyAndOrderFront(None);
    }
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

/// A push button without an action.
fn button(mtm: MainThreadMarker, title: &str) -> Retained<NSButton> {
    // SAFETY: no target, no action.
    unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm) }
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
