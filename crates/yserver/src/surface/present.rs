//! Present and RandR on the surface platform: Copy-mode Present paced by the
//! surface's frame clock, and one synthetic RandR output per window.
//!
//! MSC is the clock's tick count and UST its tick time; a parked target arms
//! the clock, whose tick wakes the core loop. There is one output, so every
//! CRTC id resolves to surface 0.

use std::io;

use yserver_core::randr::{RandrMode, RandrOutput};

use super::SurfacePlatform;
use crate::kms::render::{Platform, XBackend, single_output_randr};

impl XBackend<SurfacePlatform> {
    /// `(msc, ust)` of surface 0's clock; `(0, 0)` headless.
    pub(crate) fn surface_ust_msc(&self) -> (u64, u64) {
        self.platform
            .outputs
            .first()
            .map_or((0, 0), |output| output.clock().sample())
    }

    /// Arm surface 0's clock for the earliest of `targets`; `Ok(1)` when
    /// that moved the armed target, `Ok(0)` when it was already covered.
    pub(crate) fn arm_surface_msc(&mut self, targets: &[u64]) -> io::Result<usize> {
        let (Some(output), Some(&target)) = (self.platform.outputs.first(), targets.iter().min())
        else {
            return Ok(0);
        };
        Ok(usize::from(output.clock().arm_msc(target)))
    }

    /// No frame in flight and nothing left to compose (or nothing can show).
    pub(crate) fn surface_display_idle(&self) -> bool {
        !self.scene.has_pending_page_flips()
            && (!self.platform.outputs.iter().any(|o| !o.hidden()) || !self.scene_wants_scene())
    }

    /// The one window output, refreshing at the clock's nominal rate.
    pub(crate) fn surface_randr_outputs_and_modes(&self) -> (Vec<RandrOutput>, Vec<RandrMode>) {
        let refresh = self
            .platform
            .outputs
            .first()
            .map_or(super::clock::FALLBACK_REFRESH_HZ, |output| {
                output.clock().refresh_hz()
            });
        single_output_randr(self.platform.fb_size(), refresh)
    }
}
