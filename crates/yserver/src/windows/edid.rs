//! A monitor's EDID: its GDI device name (`\\.\DISPLAY1`) to the target's
//! device interface path through `QueryDisplayConfig`, then that device's
//! `EDID` registry value through SetupAPI.

use std::ptr;

use windows_sys::{
    Win32::{
        Devices::{
            DeviceAndDriverInstallation::{
                DICS_FLAG_GLOBAL, DIREG_DEV, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
                SetupDiCreateDeviceInfoList, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
                SetupDiOpenDevRegKey, SetupDiOpenDeviceInterfaceW,
            },
            Display::{
                DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
                DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
                DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
                GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
            },
        },
        Foundation::{ERROR_SUCCESS, INVALID_HANDLE_VALUE},
        Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFOEXW},
        System::Registry::{KEY_READ, RegCloseKey, RegQueryValueExW},
    },
    core::w,
};

/// The EDID of `monitor`; `None` when Windows does not have one (a
/// virtual or remote display).
pub(super) fn of_monitor(monitor: HMONITOR) -> Option<Vec<u8>> {
    let device = gdi_device_name(monitor)?;
    let path = monitor_device_path(&device);
    let name = |s: &[u16]| {
        String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
    };
    log::debug!(
        "windows: monitor {} is {:?}",
        name(&device),
        path.as_ref().map(|p| name(p))
    );
    edid_of_interface(&path?)
}

/// `\\.\DISPLAYn` of `monitor`, NUL-terminated.
fn gdi_device_name(monitor: HMONITOR) -> Option<[u16; 32]> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).ok()?;
    // SAFETY: a monitor handle and an EX-sized info.
    (unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) } != 0).then_some(info.szDevice)
}

/// The active target shown by the GDI source `device`: its device
/// interface path, NUL-terminated.
fn monitor_device_path(device: &[u16; 32]) -> Option<[u16; 128]> {
    let (mut paths, mut modes) = (0u32, 0u32);
    // SAFETY: writes both counts.
    let status = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &raw mut paths, &raw mut modes)
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let mut path_info = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
    let mut mode_info: Vec<DISPLAYCONFIG_MODE_INFO> =
        // SAFETY: zeroed is a valid (plain data) mode info.
        (0..modes).map(|_| unsafe { std::mem::zeroed() }).collect();
    // SAFETY: arrays of the counts given; no topology id.
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut paths,
            path_info.as_mut_ptr(),
            &raw mut modes,
            mode_info.as_mut_ptr(),
            ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    path_info.truncate(paths as usize);
    let name_len = |s: &[u16]| s.iter().position(|&c| c == 0).unwrap_or(s.len());
    let want = &device[..name_len(device)];
    path_info.iter().find_map(|path| {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = u32::try_from(size_of_val(&source)).ok()?;
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        // SAFETY: a request packet of the size it says.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut source.header) } != 0 {
            return None;
        }
        let name = &source.viewGdiDeviceName;
        if &name[..name_len(name)] != want {
            return None;
        }
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        target.header.size = u32::try_from(size_of_val(&target)).ok()?;
        target.header.adapterId = path.targetInfo.adapterId;
        target.header.id = path.targetInfo.id;
        // SAFETY: as above.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut target.header) } != 0 {
            return None;
        }
        (target.monitorDevicePath[0] != 0).then_some(target.monitorDevicePath)
    })
}

/// The `EDID` value of the device whose interface is `path`.
fn edid_of_interface(path: &[u16; 128]) -> Option<Vec<u8>> {
    // SAFETY: an empty set, destroyed below.
    let set = unsafe { SetupDiCreateDeviceInfoList(ptr::null(), ptr::null_mut()) };
    if set == INVALID_HANDLE_VALUE as isize {
        return None;
    }
    let edid = (|| {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).ok()?,
            ..Default::default()
        };
        // SAFETY: a live set and a NUL-terminated path; adds its device.
        if unsafe { SetupDiOpenDeviceInterfaceW(set, path.as_ptr(), 0, &raw mut interface) } == 0 {
            return None;
        }
        let mut device = SP_DEVINFO_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).ok()?,
            ..Default::default()
        };
        // SAFETY: the one device the set holds.
        if unsafe { SetupDiEnumDeviceInfo(set, 0, &raw mut device) } == 0 {
            return None;
        }
        // SAFETY: that device's hardware key, closed below.
        let key = unsafe {
            SetupDiOpenDevRegKey(
                set,
                &raw const device,
                DICS_FLAG_GLOBAL,
                0,
                DIREG_DEV,
                KEY_READ,
            )
        };
        if key.is_null() || key == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut data = vec![0u8; 1024];
        let mut len = u32::try_from(data.len()).ok()?;
        // SAFETY: a live key and a buffer of `len` bytes.
        let status = unsafe {
            RegQueryValueExW(
                key,
                w!("EDID"),
                ptr::null(),
                ptr::null_mut(),
                data.as_mut_ptr(),
                &raw mut len,
            )
        };
        // SAFETY: the key opened above.
        unsafe { RegCloseKey(key) };
        if status != ERROR_SUCCESS {
            return None;
        }
        data.truncate(len as usize);
        Some(data)
    })();
    // SAFETY: the set created above.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    edid
}
