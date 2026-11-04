//! The clipboard bridge's host side: the Windows clipboard's
//! `CF_UNICODETEXT`, used from the bridge's thread. The clipboard sequence
//! number is the change count, as `changeCount` is on macOS: the bridge
//! polls it, so no `WM_CLIPBOARDUPDATE` listener is needed, and our own
//! writes are known by the number they leave.

use std::{ptr, thread, time::Duration};

use windows_sys::Win32::{
    Foundation::{GlobalFree, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
    },
};

use crate::surface::clipboard::{Pasteboard, from_windows_units, to_windows_units};

/// `CF_UNICODETEXT`.
const CF_UNICODETEXT: u32 = 13;
/// Another process may hold the clipboard open for a moment.
const OPEN_TRIES: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(20);

/// The Windows clipboard's text.
pub(crate) struct WindowsClipboard {
    /// The shell's window, a thread that pumps messages: it owns what we
    /// write (`EmptyClipboard` under a null owner makes `SetClipboardData`
    /// fail, per its documentation).
    owner: usize,
    /// Reads that found the clipboard held open by another process (a
    /// clipboard manager reading the new contents, say): in the change
    /// count's high half, so the bridge's next poll sees a change and
    /// reads again instead of taking the busy clipboard for an empty one.
    busy_reads: u32,
}

impl WindowsClipboard {
    pub(crate) fn new(owner: HWND) -> Self {
        Self {
            owner: owner as usize,
            busy_reads: 0,
        }
    }

    /// Open the clipboard, retrying while another process holds it.
    fn open(&self) -> Option<Opened> {
        for _ in 0..OPEN_TRIES {
            // SAFETY: a window of this process (or null); closed by `Opened`.
            if unsafe { OpenClipboard(self.owner as HWND) } != 0 {
                return Some(Opened);
            }
            thread::sleep(OPEN_RETRY);
        }
        log::debug!("windows: the clipboard stayed open in another process");
        None
    }
}

/// The open clipboard, closed on drop.
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: opened by this thread in `open`.
        unsafe { CloseClipboard() };
    }
}

impl Pasteboard for WindowsClipboard {
    fn change_count(&mut self) -> i64 {
        // SAFETY: no arguments.
        let sequence = unsafe { GetClipboardSequenceNumber() };
        i64::from(self.busy_reads) << 32 | i64::from(sequence)
    }

    fn read_text(&mut self) -> Option<String> {
        // SAFETY: no pointers.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return None;
        }
        let Some(_open) = self.open() else {
            self.busy_reads = self.busy_reads.wrapping_add(1);
            return None;
        };
        // SAFETY: the clipboard is open; the handle stays the clipboard's.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: a global memory handle, locked while read.
        let data = unsafe { GlobalLock(handle) }.cast::<u16>();
        if data.is_null() {
            return None;
        }
        // SAFETY: as above; the block is `GlobalSize` bytes.
        let units =
            unsafe { std::slice::from_raw_parts(data, GlobalSize(handle) / size_of::<u16>()) };
        let text = from_windows_units(units);
        // SAFETY: locked above.
        unsafe { GlobalUnlock(handle) };
        Some(text)
    }

    fn write_text(&mut self, text: &str) -> i64 {
        let units = to_windows_units(text);
        let Some(open) = self.open() else {
            log::warn!("windows: the clipboard is busy; the X clipboard text is lost");
            return self.change_count();
        };
        // SAFETY: the clipboard is open by this thread.
        unsafe { EmptyClipboard() };
        if !set_text(&units) {
            log::warn!("windows: the clipboard refused the X clipboard text");
        }
        // Closed first: the number our write leaves.
        drop(open);
        self.change_count()
    }
}

/// Hand `units` to the open clipboard as `CF_UNICODETEXT`.
fn set_text(units: &[u16]) -> bool {
    let bytes = size_of_val(units);
    // SAFETY: a fresh block of `bytes`, filled under its lock; once
    // `SetClipboardData` takes it the system owns it, else we free it.
    unsafe {
        let block = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if block.is_null() {
            return false;
        }
        let data = GlobalLock(block).cast::<u16>();
        if data.is_null() {
            GlobalFree(block);
            return false;
        }
        ptr::copy_nonoverlapping(units.as_ptr(), data, units.len());
        GlobalUnlock(block);
        if SetClipboardData(CF_UNICODETEXT, block).is_null() {
            GlobalFree(block);
            return false;
        }
    }
    true
}
