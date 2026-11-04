//! yserver's icon (`assets/yserver.ico`, resource 1 of yserver.exe through
//! `yserver.rc`) at the sizes the system asks for: the class icons and the
//! tray icon. Without the resource, the stock application icon.

use std::ptr;

use windows_sys::Win32::{
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        GetSystemMetrics, HICON, IDI_APPLICATION, IMAGE_ICON, LR_SHARED, LoadIconW, LoadImageW,
        SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
    },
};

/// The resource id in `yserver.rc`.
const ICON_ID: u16 = 1;

/// The icon for title bars, the tray and the taskbar's small size.
pub(super) fn small() -> HICON {
    load(SM_CXSMICON, SM_CYSMICON)
}

/// The icon for Alt+Tab and the taskbar.
pub(super) fn large() -> HICON {
    load(SM_CXICON, SM_CYICON)
}

fn load(
    width: windows_sys::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX,
    height: windows_sys::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX,
) -> HICON {
    // SAFETY: plain queries; the resource name is an integer id
    // (`MAKEINTRESOURCEW`), and a shared icon is never destroyed.
    unsafe {
        let icon = LoadImageW(
            GetModuleHandleW(ptr::null()),
            usize::from(ICON_ID) as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(width),
            GetSystemMetrics(height),
            LR_SHARED,
        );
        if icon.is_null() {
            LoadIconW(ptr::null_mut(), IDI_APPLICATION)
        } else {
            icon
        }
    }
}
