//! The clipboard bridge's host side: `NSPasteboard generalPasteboard`, used
//! on the main thread.

use dispatch2::DispatchQueue;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

use crate::surface::clipboard::Pasteboard;

/// The general pasteboard's plain text (`public.utf8-plain-text`).
pub(crate) struct GeneralPasteboard;

/// Run `work` on the main queue and wait for its result.
fn on_main<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    let mut result = None;
    DispatchQueue::main().exec_sync(|| result = Some(work()));
    result.expect("exec_sync ran the block")
}

impl Pasteboard for GeneralPasteboard {
    fn change_count(&mut self) -> i64 {
        on_main(|| NSPasteboard::generalPasteboard().changeCount() as i64)
    }

    fn read_text(&mut self) -> Option<String> {
        on_main(|| {
            // SAFETY: an AppKit constant, valid for the process' lifetime.
            let string_type = unsafe { NSPasteboardTypeString };
            NSPasteboard::generalPasteboard()
                .stringForType(string_type)
                .map(|text| text.to_string())
        })
    }

    fn write_text(&mut self, text: &str) -> i64 {
        let text = text.to_owned();
        on_main(move || {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            // SAFETY: as in `read_text`.
            let string_type = unsafe { NSPasteboardTypeString };
            if !pasteboard.setString_forType(&NSString::from_str(&text), string_type) {
                log::warn!("macos: the pasteboard refused the X clipboard text");
            }
            pasteboard.changeCount() as i64
        })
    }
}
