//! The X display this server listens on, for the app menu's first item and
//! the Settings window: both show it and copy `export DISPLAY=:N`.

use std::cell::Cell;

use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSApplication, NSMenuItem, NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

thread_local! {
    /// Set once, on the main thread, when the server has bound its socket.
    static DISPLAY: Cell<Option<u16>> = const { Cell::new(None) };
}

/// The display as `:N`, once known.
pub(super) fn name() -> Option<String> {
    DISPLAY.get().map(|display| format!(":{display}"))
}

/// The window's title and the menu item's.
pub(super) fn title(display: u16) -> String {
    format!("yserver DISPLAY=:{display}")
}

/// Put `export DISPLAY=:N` on the general pasteboard as plain text.
pub(super) fn copy() {
    let Some(display) = name() else {
        return;
    };
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: an AppKit constant, valid for the process' lifetime.
    let string_type = unsafe { NSPasteboardTypeString };
    let text = NSString::from_str(&format!("export DISPLAY={display}"));
    if !pasteboard.setString_forType(&text, string_type) {
        log::warn!("macos: the pasteboard refused the DISPLAY line");
    }
}

/// Remember `display` and put its copy item first in the app menu.
pub(super) fn announce(mtm: MainThreadMarker, display: u16) {
    DISPLAY.set(Some(display));
    let Some(menu) = NSApplication::sharedApplication(mtm)
        .mainMenu()
        .and_then(|bar| bar.itemAtIndex(0))
        .and_then(|item| item.submenu())
    else {
        return;
    };
    // SAFETY: `copyDisplay:` is an app delegate action taking a sender; the
    // item has no target, so it goes up the responder chain.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&title(display)),
            Some(sel!(copyDisplay:)),
            &NSString::from_str(""),
        )
    };
    menu.insertItem_atIndex(&item, 0);
    menu.insertItem_atIndex(&NSMenuItem::separatorItem(mtm), 1);
}
