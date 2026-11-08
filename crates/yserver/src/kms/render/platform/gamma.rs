use super::PlatformBackend;
use crate::drm::{
    atomic::{AtomicCommitEvent, AtomicCommitKind, AtomicCommitRequest},
    modeset::{
        append_pending_crtc_properties, finish_pending_gamma_lut, finish_pending_vrr_change,
    },
};

impl PlatformBackend {
    /// Submit an idle output's pending gamma ramp or due VRR clear as an
    /// event-producing CRTC commit. VRR clears win over gamma; enables remain
    /// attached to their next direct frame.
    pub(crate) fn submit_pending_crtc_property_for_output(
        &self,
        output_idx: usize,
        vrr_wanted: bool,
        now: std::time::Instant,
        current_primary_framebuffer: Option<::drm::control::framebuffer::Handle>,
    ) -> std::io::Result<()> {
        let Some(layout) = self.outputs.get(output_idx) else {
            return Ok(());
        };
        let Some(device) = self.device_for_key(layout.key.device_key) else {
            return Ok(());
        };
        let output = &layout.output;
        let gamma_pending = output.pending_gamma_lut.borrow().is_some();
        let vrr_clear_may_be_due = !vrr_wanted && output.vrr_committed.get();
        if !gamma_pending && !vrr_clear_may_be_due {
            return Ok(());
        }
        if device
            .atomic_commit_sink
            .page_flip_commit_in_flight(&device.device, output.crtc)
        {
            if vrr_clear_may_be_due {
                output.vrr_clear_retry_deadline.set(None);
                output.vrr_clear_waiting_for_flip.set(true);
            }
            return Ok(());
        }

        let mut request = AtomicCommitRequest::new();
        let changes = append_pending_crtc_properties(
            &device.device,
            &device.atomic_commit_sink,
            &mut request,
            [(output, vrr_wanted)],
        );
        if changes.gamma_blobs.is_empty() && changes.vrr_changes.is_empty() {
            return Ok(());
        }
        let carries_vrr_clear = changes.vrr_changes.iter().any(|(_, value)| !*value);
        if carries_vrr_clear && current_primary_framebuffer.is_none() {
            log::debug!(
                "kms: standalone VRR clear skipped output={} because no live direct framebuffer is available",
                layout.key.connector_name
            );
            return Ok(());
        }
        if carries_vrr_clear && let Some(framebuffer) = current_primary_framebuffer {
            request.add_raw_property(
                output.plane.into(),
                output.plane_fb_id_prop,
                u64::from(u32::from(framebuffer)),
            );
        }
        if carries_vrr_clear {
            output.vrr_clear_retry_deadline.set(None);
            output.vrr_clear_waiting_for_flip.set(false);
        }
        let flags = ::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT
            | ::drm::control::AtomicCommitFlags::NONBLOCK;
        let events = [AtomicCommitEvent {
            crtc: output.crtc,
            motion_timestamp: None,
        }];
        let result = if !changes.gamma_blobs.is_empty() {
            debug_assert!(changes.vrr_changes.is_empty());
            let blob = changes.gamma_blobs[0].1;
            let result = device.atomic_commit_sink.submit_with_commit_id(
                &device.device,
                flags,
                AtomicCommitKind::GammaOnly,
                request,
                &events,
            );
            finish_pending_gamma_lut(
                &device.device,
                &device.atomic_commit_sink,
                output,
                blob,
                &result,
                true,
            );
            result.map(|_| ())
        } else {
            let result = device.atomic_commit_sink.submit_with_commit_id(
                &device.device,
                flags,
                AtomicCommitKind::VrrOnly,
                request,
                &events,
            );
            for (vrr_output, value) in &changes.vrr_changes {
                // A standalone clear has no frame request to retry without
                // CRTC properties; finalize EINVAL through the VRR latch.
                finish_pending_vrr_change(vrr_output, *value, &result, true);
            }
            result.map(|_| ())
        };

        match result {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) && carries_vrr_clear => {
                output.vrr_clear_retry_deadline.set(None);
                output.vrr_clear_waiting_for_flip.set(true);
                Ok(())
            }
            Err(error)
                if carries_vrr_clear
                    && error.raw_os_error() != Some(libc::EINVAL)
                    && error.raw_os_error() != Some(libc::EBUSY) =>
            {
                output.vrr_clear_waiting_for_flip.set(false);
                output.vrr_clear_retry_deadline.set(Some(
                    now + crate::drm::modeset::output_refresh_period(output),
                ));
                Err(error)
            }
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) && carries_vrr_clear => {
                output.vrr_clear_retry_deadline.set(None);
                output.vrr_clear_waiting_for_flip.set(false);
                Ok(())
            }
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            Err(error)
                if error.raw_os_error() == Some(libc::EINVAL)
                    && output.pending_gamma_lut.borrow().is_none() =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Retry a gamma ramp at its existing deadline or a failed VRR clear at
    /// its event/deadline boundary. The shared assembler preserves one
    /// optional CRTC property class per request.
    pub(crate) fn retry_pending_crtc_property_after_flip(
        &self,
        output_idx: usize,
        now: std::time::Instant,
        vrr_wanted: bool,
        current_primary_framebuffer: Option<::drm::control::framebuffer::Handle>,
    ) {
        let Some(output) = self.outputs.get(output_idx).map(|layout| &layout.output) else {
            return;
        };
        let gamma_due = output.pending_gamma_lut.borrow().is_some()
            && output
                .pending_gamma_deadline
                .get()
                .is_none_or(|deadline| now >= deadline);
        let vrr_clear_due = !vrr_wanted
            && output.vrr_committed.get()
            && (output.vrr_clear_waiting_for_flip.get()
                || output
                    .vrr_clear_retry_deadline
                    .get()
                    .is_some_and(|deadline| now >= deadline));
        if !output.vrr_committed.get() {
            output.vrr_clear_retry_deadline.set(None);
            output.vrr_clear_waiting_for_flip.set(false);
        }
        if !gamma_due && !vrr_clear_due {
            return;
        }
        if gamma_due
            && output
                .pending_gamma_deadline
                .get()
                .is_some_and(|deadline| now >= deadline)
        {
            output.pending_gamma_deadline.set(None);
        }
        if vrr_clear_due {
            output.vrr_clear_retry_deadline.set(None);
            output.vrr_clear_waiting_for_flip.set(false);
        }
        if let Err(error) = self.submit_pending_crtc_property_for_output(
            output_idx,
            vrr_wanted,
            now,
            current_primary_framebuffer,
        ) {
            log::warn!(
                "kms: standalone CRTC property retry for CRTC {:?} failed: {error}",
                output.crtc
            );
        }
    }

    /// Scene, direct-scanout, and composed scheduling share this gate so an
    /// accepted CRTC-property-only event commit owns the CRTC until its event.
    pub(crate) fn scanout_commit_in_flight_for_output(&self, output_idx: usize) -> bool {
        if self.cursor_commit_in_flight_for_output(output_idx) {
            return true;
        }
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .atomic_commit_sink
                    .page_flip_commit_in_flight(&device.device, layout.output.crtc)
            })
    }

    pub(crate) fn scanout_commit_in_flight_for_outputs(&self, output_indices: &[usize]) -> bool {
        output_indices
            .iter()
            .any(|output_idx| self.scanout_commit_in_flight_for_output(*output_idx))
    }
}
