//! Present and RandR on the surface platform: Copy-mode Present paced by the
//! surface's frame clock, and one synthetic RandR output per window.
//!
//! MSC is the clock's tick count and UST its tick time; a parked target arms
//! the clock, whose tick wakes the core loop. There is one output, so every
//! CRTC id resolves to surface 0.

use std::io;

use yserver_core::{
    randr::{RR_ROTATE_0, RandrMode, RandrOutput, RandrState},
    server::ServerState,
};

use super::SurfacePlatform;
use crate::kms::render::XBackend;

/// The window's fixed modes, offered when they fit the platform's largest
/// output.
const MODE_SIZES: [(u16, u16); 8] = [
    (2560, 1600),
    (2560, 1440),
    (1920, 1200),
    (1920, 1080),
    (1680, 1050),
    (1440, 900),
    (1280, 800),
    (1024, 768),
];
/// The mode of a current size off the fixed list; fixed mode `i` is
/// `FIRST_FIXED_MODE_ID + i`.
const CUSTOM_MODE_ID: u32 = 3;
const FIRST_FIXED_MODE_ID: u32 = 4;

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
        window_randr(
            self.platform.output_size,
            self.platform.output_mm,
            self.platform.max_output,
            refresh,
        )
    }

    /// Rebuild `state.randr` from the window output, keeping the screen
    /// size, the limits and (unless the mode list changed) lastConfigTime;
    /// lastSetTime becomes `set_time`.
    pub(crate) fn surface_refresh_randr(&self, state: &mut ServerState, set_time: u32) {
        let previous = &state.randr;
        let screen = (
            previous.screen_width,
            previous.screen_height,
            previous.width_mm,
            previous.height_mm,
        );
        let (limits, config_timestamp) = (previous.limits, previous.config_timestamp);
        let old_modes: Vec<(u32, u16, u16)> = previous
            .mode_table
            .iter()
            .map(|m| (m.mode_id, m.width, m.height))
            .collect();
        let (outputs, modes) = self.surface_randr_outputs_and_modes();
        let modes_changed = old_modes
            != modes
                .iter()
                .map(|m| (m.mode_id, m.width, m.height))
                .collect::<Vec<_>>();
        state.randr = RandrState::from_outputs_with_modes(set_time, outputs, modes);
        let randr = &mut state.randr;
        (
            randr.screen_width,
            randr.screen_height,
            randr.width_mm,
            randr.height_mm,
        ) = screen;
        randr.limits = limits;
        // RROutputSetModes marks the configuration changed (lastConfigTime).
        randr.config_timestamp = if modes_changed {
            set_time.max(1)
        } else {
            config_timestamp
        };
    }
}

/// RandR for the window: one output `WINDOW-0` on CRTC 2 at (0,0) showing
/// `size` over `mm` millimetres, which is its preferred mode, followed by
/// the fixed modes that fit `max_output`.
#[must_use]
pub(crate) fn window_randr(
    size: (u16, u16),
    mm: (u32, u32),
    max_output: (u16, u16),
    vrefresh: u32,
) -> (Vec<RandrOutput>, Vec<RandrMode>) {
    let fixed_id = |i: usize| FIRST_FIXED_MODE_ID + u32::try_from(i).unwrap_or(u32::MAX);
    let mode_id = MODE_SIZES
        .iter()
        .position(|&mode| mode == size)
        .map_or(CUSTOM_MODE_ID, fixed_id);
    let mut modes = vec![RandrMode {
        mode_id,
        width: size.0,
        height: size.1,
        vrefresh,
        timing: None,
    }];
    modes.extend(
        MODE_SIZES
            .iter()
            .enumerate()
            .filter(|&(_, &(w, h))| (w, h) != size && w <= max_output.0 && h <= max_output.1)
            .map(|(i, &(width, height))| RandrMode {
                mode_id: fixed_id(i),
                width,
                height,
                vrefresh,
                timing: None,
            }),
    );
    let outputs = vec![RandrOutput {
        name: "WINDOW-0".to_string(),
        output_id: 1,
        crtc_id: 2,
        mode_id,
        connected: true,
        x: 0,
        y: 0,
        width: size.0,
        height: size.1,
        vrefresh,
        timing: None,
        // 0: unknown; `RandrState::output_info` synthesises 96 DPI.
        mm_width: mm.0,
        mm_height: mm.1,
        mode_ids: modes.iter().map(|m| m.mode_id).collect(),
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: RR_ROTATE_0,
    }];
    (outputs, modes)
}
