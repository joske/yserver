//! DRM display hotplug detection for the KMS backend.
//!
//! Linux-only: a udev monitor on the `drm` subsystem exposes a pollable
//! fd. The backend drains it and debounces the expensive reprobe work.

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, RawFd};

#[cfg(target_os = "linux")]
pub(crate) struct DrmHotplugMonitor {
    socket: udev::MonitorSocket,
}

#[cfg(target_os = "linux")]
impl DrmHotplugMonitor {
    pub(crate) fn new() -> std::io::Result<Option<Self>> {
        let builder = match udev::MonitorBuilder::new() {
            Ok(builder) => builder,
            Err(e) => {
                log::warn!("drm hotplug: udev monitor unavailable: {e}; hotplug disabled");
                return Ok(None);
            }
        };
        let socket = builder.match_subsystem("drm")?.listen()?;
        log::info!("drm hotplug: udev monitor listening on drm subsystem");
        Ok(Some(Self { socket }))
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    pub(crate) fn drain(&mut self) -> bool {
        let mut saw_change = false;
        for event in self.socket.iter() {
            let action = event.action().and_then(|a| a.to_str()).unwrap_or("");
            log::debug!("drm hotplug: uevent action={action:?}");
            if matches!(action, "add" | "remove" | "change") {
                saw_change = true;
            }
        }
        saw_change
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn monitor_opens_and_drains_without_blocking() {
        match super::DrmHotplugMonitor::new() {
            Ok(Some(mut monitor)) => {
                assert!(monitor.raw_fd() >= 0);
                let _ = monitor.drain();
            }
            Ok(None) => {}
            Err(e) => panic!("monitor construction errored unexpectedly: {e}"),
        }
    }
}
