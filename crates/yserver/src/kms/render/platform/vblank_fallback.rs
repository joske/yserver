//! Vblank sources for KMS devices that reject `DRM_IOCTL_CRTC_QUEUE_SEQUENCE`.
//!
//! Xorg modesetting falls back to the legacy `DRM_IOCTL_WAIT_VBLANK` for both
//! MSC queries and queued vblank events (`vblank.c`, `ms_queue_vblank`). When
//! a device supports neither, Present's fake vblank (`present_fake.c`) keeps
//! time with a timer; here a per-CRTC deadline at the mode's refresh interval
//! plays that role, so an armed wait always produces a clock advance.

use super::{CrtcKey, PlatformBackend, flip_events::PageFlipEventSource};
use std::{
    collections::{HashMap, HashSet},
    io,
    time::{Duration, Instant},
};

/// One armed software vblank: when it fires, and the general MSC it was armed
/// against. Another source advancing that MSC first makes the tick redundant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SoftwareVblank {
    pub(crate) deadline: Instant,
    pub(crate) armed_msc: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum VblankArmSlot {
    Idle,
    SharedWake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrozenClock {
    pub(crate) anchor_msc: u64,
    pub(crate) anchor_ust: u64,
    pub(crate) anchor_at: Instant,
    pub(crate) interval: Duration,
}

#[derive(Default)]
pub(crate) struct VblankFallback {
    /// Devices whose legacy `DRM_IOCTL_WAIT_VBLANK` is unavailable; latched
    /// for the process lifetime like the sequence latch.
    pub(crate) wait_vblank_unsupported_devices: HashSet<crate::platform::drm::DrmDeviceKey>,
    /// Legacy vblank pipe (CRTC index in the device's resource list).
    pub(crate) pipes: HashMap<CrtcKey, u32>,
    /// Armed software vblanks for devices with no kernel vblank source.
    pub(crate) software: HashMap<CrtcKey, SoftwareVblank>,
    /// Time each unfired kernel arm slot was successfully issued.
    pub(crate) kernel_arm_watch: HashMap<(CrtcKey, VblankArmSlot), Instant>,
    /// CRTC clocks whose accepted kernel vblank arm did not fire in time.
    pub(crate) frozen: HashMap<CrtcKey, FrozenClock>,
}

impl VblankFallback {
    pub(super) fn retain_live(&mut self, live: &HashSet<CrtcKey>) {
        self.pipes.retain(|key, _| live.contains(key));
        self.software.retain(|key, _| live.contains(key));
        self.kernel_arm_watch
            .retain(|(key, _), _| live.contains(key));
        self.frozen.retain(|key, _| live.contains(key));
    }
}

/// Errors that mean the device has no legacy vblank support at all.
pub(crate) fn wait_vblank_unsupported_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL)
    )
}

/// The mode's frame period; 60 Hz when the timing is unknown.
pub(crate) fn mode_frame_interval(mode: &crate::platform::drm::Mode) -> Duration {
    let total = u64::from(mode.htotal) * u64::from(mode.vtotal);
    if mode.clock_khz > 0 && total > 0 {
        return Duration::from_nanos(total * 1_000_000 / u64::from(mode.clock_khz));
    }
    if mode.vrefresh > 0 {
        return Duration::from_nanos(1_000_000_000 / u64::from(mode.vrefresh));
    }
    Duration::from_nanos(1_000_000_000 / 60)
}

impl PlatformBackend {
    pub(crate) fn wait_vblank_supported(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        !self
            .vblank_fallback
            .wait_vblank_unsupported_devices
            .contains(&device_key)
    }

    /// Latch legacy vblank off for `device_key` on an unsupported-device
    /// error; returns whether it latched now.
    pub(crate) fn note_wait_vblank_error(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        error: &io::Error,
    ) -> bool {
        if !wait_vblank_unsupported_error(error) {
            return false;
        }
        let newly = self
            .vblank_fallback
            .wait_vblank_unsupported_devices
            .insert(device_key);
        if newly {
            log::warn!(
                "DRM_IOCTL_WAIT_VBLANK failed on {device_key} ({error}) — no kernel vblank \
                 events on that device; Present waits use a software vblank"
            );
        }
        newly
    }

    /// Cached legacy pipe of `crtc_key`, resolved from the device on a miss.
    pub(crate) fn legacy_vblank_pipe(&mut self, crtc_key: CrtcKey) -> Option<u32> {
        if let Some(&pipe) = self.vblank_fallback.pipes.get(&crtc_key) {
            return Some(pipe);
        }
        let device = self.device_for_key(crtc_key.device_key)?;
        let pipe = crate::drm::page_flip::crtc_pipe_index(&device.device, crtc_key.crtc)?;
        self.vblank_fallback.pipes.insert(crtc_key, pipe);
        Some(pipe)
    }

    /// Real vblank count for a flip whose event carried `frame == 0`, via
    /// the legacy query (Xorg `ms_get_kernel_ust_msc`).
    pub(super) fn query_legacy_vblank_msc(
        &mut self,
        crtc_key: CrtcKey,
        device: &crate::drm::Device,
        source: &dyn PageFlipEventSource,
    ) -> Option<u64> {
        if !self.wait_vblank_supported(crtc_key.device_key) {
            return None;
        }
        let Some(pipe) = self.legacy_vblank_pipe(crtc_key) else {
            self.note_wait_vblank_error(
                crtc_key.device_key,
                &io::Error::from_raw_os_error(libc::EINVAL),
            );
            return None;
        };
        match source.query_vblank(device, pipe) {
            Ok((count, _)) => Some(u64::from(count)),
            Err(error) => {
                self.note_wait_vblank_error(crtc_key.device_key, &error);
                None
            }
        }
    }

    /// Frame period of the output driving `crtc_key`.
    pub(crate) fn crtc_frame_interval(&self, crtc_key: CrtcKey) -> Option<Duration> {
        let output_idx = self.output_index_for_crtc(crtc_key)?;
        Some(mode_frame_interval(&self.outputs[output_idx].output.picked))
    }

    /// Arm one software vblank a frame period from `now`; `false` when one is
    /// already armed or the CRTC has no live output.
    pub(crate) fn arm_software_vblank(&mut self, crtc_key: CrtcKey, now: Instant) -> bool {
        if self.vblank_fallback.software.contains_key(&crtc_key) {
            return false;
        }
        let Some(interval) = self.crtc_frame_interval(crtc_key) else {
            return false;
        };
        let armed_msc = self.present_get_ust_msc(crtc_key).0;
        let deadline =
            self.vblank_fallback
                .frozen
                .get(&crtc_key)
                .map_or(now + interval, |frozen| {
                    let elapsed = now.saturating_duration_since(frozen.anchor_at);
                    let interval_ns = frozen.interval.as_nanos().max(1);
                    let frames = elapsed.as_nanos() / interval_ns;
                    let next_ns = (frames + 1).saturating_mul(interval_ns);
                    frozen.anchor_at
                        + Duration::from_nanos(u64::try_from(next_ns).unwrap_or(u64::MAX))
                });
        self.vblank_fallback.software.insert(
            crtc_key,
            SoftwareVblank {
                deadline,
                armed_msc,
            },
        );
        true
    }

    pub(crate) fn next_software_vblank_deadline(&self) -> Option<Instant> {
        self.vblank_fallback
            .software
            .values()
            .map(|vblank| vblank.deadline)
            .min()
    }

    pub(crate) fn frozen_clock_now(&self, crtc_key: CrtcKey, now: Instant) -> Option<(u64, u64)> {
        let frozen = self.vblank_fallback.frozen.get(&crtc_key)?;
        let elapsed = now.saturating_duration_since(frozen.anchor_at);
        let interval_ns = frozen.interval.as_nanos().max(1);
        let frames = elapsed.as_nanos() / interval_ns;
        let frames_u64 = u64::try_from(frames).unwrap_or(u64::MAX);
        let elapsed_us =
            u64::try_from(frames.saturating_mul(interval_ns) / 1_000).unwrap_or(u64::MAX);
        Some((
            frozen.anchor_msc.saturating_add(frames_u64),
            frozen.anchor_ust.saturating_add(elapsed_us),
        ))
    }

    pub(crate) fn next_frozen_watch_deadline(&self) -> Option<Instant> {
        self.vblank_fallback
            .kernel_arm_watch
            .iter()
            .filter(|((key, _), _)| !self.vblank_fallback.frozen.contains_key(key))
            .map(|(_, armed_at)| {
                *armed_at + crate::kms::render::backend::KmsBackend::FROZEN_VBLANK_WINDOW
            })
            .min()
    }

    /// Remove and return the software vblanks due at `now`.
    pub(crate) fn take_due_software_vblanks(
        &mut self,
        now: Instant,
    ) -> Vec<(CrtcKey, SoftwareVblank)> {
        let due: Vec<(CrtcKey, SoftwareVblank)> = self
            .vblank_fallback
            .software
            .iter()
            .filter(|(_, vblank)| vblank.deadline <= now)
            .map(|(key, vblank)| (*key, *vblank))
            .collect();
        for (key, _) in &due {
            self.vblank_fallback.software.remove(key);
        }
        due
    }

    /// Next software MSC for `crtc_key`, shared with the `frame == 0` flip
    /// counter so neither source moves the clock backwards.
    pub(crate) fn advance_software_msc(&mut self, crtc_key: CrtcKey) -> u64 {
        let general = self.present_get_ust_msc(crtc_key).0;
        let software = self.software_msc.get(&crtc_key).copied().unwrap_or(0);
        let next = general.max(software).saturating_add(1);
        self.software_msc.insert(crtc_key, next);
        next
    }
}
