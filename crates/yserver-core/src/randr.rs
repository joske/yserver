use std::collections::HashSet;

use yserver_protocol::x11::randr as proto;

/// One RANDR output (1 connector, 1 CRTC, 1 mode in the current model).
#[derive(Debug, Clone)]
pub struct RandrOutput {
    pub name: String,
    pub output_id: u32,
    pub crtc_id: u32,
    pub mode_id: u32,
    pub connected: bool,
    /// Position in the virtual screen (placed horizontally in the
    /// current phase).
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
    /// EDID-derived physical dimensions in millimeters. 0 means
    /// unknown (e.g. virtio-gpu, displays without EDID, ynest nested
    /// backend) — `output_info` falls back to a 96-DPI synthesis from
    /// `width`/`height` in that case.
    pub mm_width: u32,
    pub mm_height: u32,
}

/// One unique mode (deduped by `(width, height, vrefresh)`).
#[derive(Debug, Clone)]
pub struct RandrMode {
    pub mode_id: u32,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
}

#[derive(Debug)]
pub struct RandrState {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub outputs: Vec<RandrOutput>,
    /// Deduped modes referenced by `outputs[i].mode_id`.
    pub modes: Vec<RandrMode>,
    /// First output's `output_id` (or 0 if outputs is empty — should
    /// not happen post-init).
    pub primary_output: u32,
    /// Aggregated virtual-screen extent (max of `x + width`).
    pub screen_width: u16,
    /// Aggregated virtual-screen extent (max of `height`).
    pub screen_height: u16,
    /// Derived from screen dimensions at 96 DPI, clamped to at least 1 mm.
    pub width_mm: u32,
    pub height_mm: u32,
}

impl RandrState {
    /// Build a `RandrState` from a vec of pre-allocated outputs.
    ///
    /// The caller is responsible for picking output / CRTC / mode IDs
    /// per spec §2.6.1: outputs `1..=N`, CRTCs `(N+1)..=2N`, modes
    /// `2N+1..` with dedup by `(width, height, vrefresh)`. `from_outputs`
    /// trusts the caller's mode-id assignment and just collects the
    /// unique `(mode_id, w, h, vrefresh)` tuples for the `modes`
    /// vector.
    ///
    /// Aggregation:
    /// - `screen_width = max(output.x + output.width)`
    /// - `screen_height = max(output.height)` (outputs are placed
    ///   horizontally in this phase, so y is 0)
    /// - `*_mm` derived from screen_* at 96 DPI
    /// - `primary_output = outputs[0].output_id` (0 if empty)
    #[must_use]
    pub fn from_outputs(timestamp: u32, outputs: Vec<RandrOutput>) -> Self {
        // Some compositors compare the first RANDR resource timestamp with
        // their own "last SetCrtcConfig" timestamp, which starts at zero.
        // Advertising zero here makes the initial server state look like a
        // completed client-side reconfiguration.
        let timestamp = timestamp.max(1);
        let screen_width: u16 = outputs
            .iter()
            .map(|o| {
                let r = i32::from(o.x).saturating_add(i32::from(o.width));
                u16::try_from(r.max(0)).unwrap_or(u16::MAX)
            })
            .max()
            .unwrap_or(0);
        let screen_height: u16 = outputs.iter().map(|o| o.height).max().unwrap_or(0);
        // mm = px * 25.4 / 96; integer form: (px*254 + 480) / 960. Previous
        // divisor was off by 10× and made GTK auto-scale at extreme factors.
        let width_mm = ((u32::from(screen_width) * 254 + 480) / 960).max(1);
        let height_mm = ((u32::from(screen_height) * 254 + 480) / 960).max(1);

        // Collect unique modes preserving caller-allocated mode_ids.
        let mut modes: Vec<RandrMode> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        for out in outputs.iter().filter(|o| o.connected) {
            if seen.insert(out.mode_id) {
                modes.push(RandrMode {
                    mode_id: out.mode_id,
                    width: out.width,
                    height: out.height,
                    vrefresh: out.vrefresh,
                });
            }
        }

        let primary_output = outputs
            .iter()
            .find(|o| o.connected)
            .or_else(|| outputs.first())
            .map_or(0, |o| o.output_id);

        Self {
            timestamp,
            config_timestamp: timestamp,
            outputs,
            modes,
            primary_output,
            screen_width,
            screen_height,
            width_mm,
            height_mm,
        }
    }

    /// Create a `RandrState` for a nested (embedded) display of the given pixel dimensions.
    ///
    /// Builds a single synthetic output with the historical IDs
    /// (output=1, crtc=2, mode=3) and name `"ynest-0"` so xts wire
    /// fixtures keep matching.
    #[must_use]
    pub fn nested(timestamp: u32, width: u16, height: u16) -> Self {
        let synthetic = RandrOutput {
            name: "ynest-0".to_string(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width,
            height,
            vrefresh: 60,
            // Nested backend has no EDID; `output_info` falls back to
            // 96-DPI synthesis from pixel dimensions.
            mm_width: 0,
            mm_height: 0,
        };
        Self::from_outputs(timestamp, vec![synthetic])
    }

    /// Returns `(min_width, min_height, max_width, max_height)`.
    ///
    /// Permissive static bounds matching Xorg/Xwayland convention.
    /// SetScreenSize isn't implemented, so this is informational —
    /// the narrow `min == max == current` range we used to advertise
    /// made mate-settings-daemon reject its own uninitialized
    /// (1, 1) stored monitor config on first boot.
    #[must_use]
    pub fn screen_size_range(&self) -> (u16, u16, u16, u16) {
        (1, 1, 16384, 16384)
    }

    /// Resize the (single) ynest output. Multi-output reconfigure is
    /// not supported here.
    pub fn resize(&mut self, timestamp: u32, width: u16, height: u16) {
        if let Some(out) = self.outputs.first().cloned() {
            let new_out = RandrOutput {
                width,
                height,
                ..out
            };
            *self = Self::from_outputs(timestamp, vec![new_out]);
        } else {
            *self = Self::nested(timestamp, width, height);
        }
    }

    /// Build a `ScreenResources` reply describing every output / CRTC /
    /// mode currently configured.
    #[must_use]
    pub fn screen_resources_current(&self) -> proto::ScreenResources {
        let crtcs: Vec<u32> = self.outputs.iter().map(|o| o.crtc_id).collect();
        let outputs: Vec<u32> = self.outputs.iter().map(|o| o.output_id).collect();

        let mut mode_names: Vec<u8> = Vec::new();
        let mut mode_infos: Vec<proto::ModeInfo> = Vec::with_capacity(self.modes.len());
        for m in &self.modes {
            let name = format!("{}x{}", m.width, m.height).into_bytes();
            #[allow(clippy::cast_possible_truncation)]
            let name_len = name.len() as u16;
            mode_infos.push(proto::ModeInfo {
                id: m.mode_id,
                width: m.width,
                height: m.height,
                dot_clock: u32::from(m.width) * u32::from(m.height) * m.vrefresh,
                hsync_start: m.width + 40,
                hsync_end: m.width + 168,
                htotal: m.width + 264,
                hskew: 0,
                vsync_start: m.height + 1,
                vsync_end: m.height + 4,
                vtotal: m.height + 28,
                name_len,
                mode_flags: 0,
            });
            mode_names.extend_from_slice(&name);
        }
        proto::ScreenResources {
            timestamp: self.timestamp,
            config_timestamp: self.config_timestamp,
            crtcs,
            outputs,
            modes: mode_infos,
            mode_names,
        }
    }

    /// Look up output info by `output_id`.
    #[must_use]
    pub fn output_info(
        &self,
        output_id: u32,
        config_timestamp: u32,
    ) -> Option<OutputInfoReplyData> {
        let _ = config_timestamp; // accepted but not used
        let out = self.outputs.iter().find(|o| o.output_id == output_id)?;
        let (width_mm, height_mm) = if out.connected {
            // Prefer the EDID-reported physical size (passed through
            // from the DRM connector); fall back to a 96-DPI synthesis
            // from pixel dimensions when the connector reports 0
            // (virtio-gpu, ynest nested, displays without EDID).
            let width_mm = if out.mm_width > 0 {
                out.mm_width
            } else {
                ((u32::from(out.width) * 254 + 480) / 960).max(1)
            };
            let height_mm = if out.mm_height > 0 {
                out.mm_height
            } else {
                ((u32::from(out.height) * 254 + 480) / 960).max(1)
            };
            (width_mm, height_mm)
        } else {
            (0, 0)
        };
        Some(OutputInfoReplyData {
            timestamp: self.timestamp,
            crtc: if out.connected { out.crtc_id } else { 0 },
            mode_id: out.mode_id,
            width_mm,
            height_mm,
            name: out.name.clone(),
            connection: if out.connected { 0 } else { 1 },
        })
    }

    /// Look up CRTC info by `crtc_id`.
    #[must_use]
    pub fn crtc_info(&self, crtc_id: u32, config_timestamp: u32) -> Option<CrtcInfoData> {
        let _ = config_timestamp;
        let out = self.outputs.iter().find(|o| o.crtc_id == crtc_id)?;
        Some(CrtcInfoData {
            timestamp: self.timestamp,
            x: out.x,
            y: out.y,
            width: out.width,
            height: out.height,
            mode_id: out.mode_id,
            output_id: out.output_id,
        })
    }
}

/// Data returned by [`RandrState::output_info`].
pub struct OutputInfoReplyData {
    pub timestamp: u32,
    pub crtc: u32,
    pub mode_id: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub name: String,
    pub connection: u8,
}

/// Data returned by [`RandrState::crtc_info`].
pub struct CrtcInfoData {
    pub timestamp: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub mode_id: u32,
    pub output_id: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_constructor_dimensions() {
        // 800x600 at 96 DPI:
        //   width_mm  = (800*254 + 480) / 960 = 212  (real: 800*25.4/96 = 211.67)
        //   height_mm = (600*254 + 480) / 960 = 159  (real: 600*25.4/96 = 158.75)
        let state = RandrState::nested(42, 800, 600);
        assert_eq!(state.screen_width, 800);
        assert_eq!(state.screen_height, 600);
        assert_eq!(state.width_mm, 212);
        assert_eq!(state.height_mm, 159);
        assert_eq!(state.timestamp, 42);
        assert_eq!(state.config_timestamp, 42);
    }

    #[test]
    fn nested_preserves_legacy_ids_and_name() {
        let state = RandrState::nested(0, 800, 600);
        assert_eq!(state.outputs.len(), 1);
        let out = &state.outputs[0];
        assert_eq!(out.output_id, 1);
        assert_eq!(out.crtc_id, 2);
        assert_eq!(out.mode_id, 3);
        assert_eq!(out.name, "ynest-0");
        assert_eq!(out.x, 0);
        assert_eq!(out.y, 0);
    }

    #[test]
    fn nested_clamps_zero_timestamp() {
        let state = RandrState::nested(0, 800, 600);
        assert_eq!(state.timestamp, 1);
        assert_eq!(state.config_timestamp, 1);

        let resources = state.screen_resources_current();
        assert_eq!(resources.timestamp, 1);
        assert_eq!(resources.config_timestamp, 1);
    }

    #[test]
    fn unknown_output_returns_none() {
        let state = RandrState::nested(0, 800, 600);
        assert!(state.output_info(99, 0).is_none());
    }

    #[test]
    fn unknown_crtc_returns_none() {
        let state = RandrState::nested(0, 800, 600);
        assert!(state.crtc_info(99, 0).is_none());
    }

    #[test]
    fn screen_resources_current_ids() {
        let state = RandrState::nested(0, 800, 600);
        let res = state.screen_resources_current();
        assert_eq!(res.crtcs, vec![2]);
        assert_eq!(res.outputs, vec![1]);
        assert_eq!(res.modes[0].id, 3);
    }

    #[test]
    fn from_outputs_aggregates_screen_extent() {
        let outs = vec![
            RandrOutput {
                name: "HDMI-1".into(),
                output_id: 1,
                crtc_id: 3,
                mode_id: 5,
                connected: true,
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
            RandrOutput {
                name: "HDMI-2".into(),
                output_id: 2,
                crtc_id: 4,
                mode_id: 6,
                connected: true,
                x: 1024,
                y: 0,
                width: 1280,
                height: 1024,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.screen_width, 2304);
        assert_eq!(st.screen_height, 1024);
        let expect_w = (2304u32 * 254 + 480) / 960;
        let expect_h = (1024u32 * 254 + 480) / 960;
        assert_eq!(st.width_mm, expect_w);
        assert_eq!(st.height_mm, expect_h);
    }

    #[test]
    fn from_outputs_dedups_shared_modes() {
        // Both outputs share mode_id 5 (caller pre-deduped).
        let outs = vec![
            RandrOutput {
                name: "A".into(),
                output_id: 1,
                crtc_id: 3,
                mode_id: 5,
                connected: true,
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
            RandrOutput {
                name: "B".into(),
                output_id: 2,
                crtc_id: 4,
                mode_id: 5,
                connected: true,
                x: 1024,
                y: 0,
                width: 1024,
                height: 768,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.modes.len(), 1);
    }

    #[test]
    fn from_outputs_distinct_modes_when_resolutions_differ() {
        let outs = vec![
            RandrOutput {
                name: "A".into(),
                output_id: 1,
                crtc_id: 3,
                mode_id: 5,
                connected: true,
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
            RandrOutput {
                name: "B".into(),
                output_id: 2,
                crtc_id: 4,
                mode_id: 6,
                connected: true,
                x: 1024,
                y: 0,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.modes.len(), 2);
    }

    #[test]
    fn output_info_uses_edid_mm_when_present() {
        let outs = vec![RandrOutput {
            name: "DP-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            // 597 × 336 mm for 2560 × 1440 ≈ 109 DPI (typical EDID
            // for the user's monitors).
            mm_width: 597,
            mm_height: 336,
        }];
        let st = RandrState::from_outputs(0, outs);
        let info = st.output_info(1, 0).expect("output 1 exists");
        assert_eq!(info.width_mm, 597, "should pass EDID width verbatim");
        assert_eq!(info.height_mm, 336, "should pass EDID height verbatim");
    }

    #[test]
    fn output_info_falls_back_to_96dpi_when_no_edid() {
        let outs = vec![RandrOutput {
            name: "ynest-0".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
        }];
        let st = RandrState::from_outputs(0, outs);
        let info = st.output_info(1, 0).expect("output 1 exists");
        // 96-DPI synthesis: 2560 * 25.4 / 96 = 677.3 → 677.
        assert_eq!(info.width_mm, 677);
        assert_eq!(info.height_mm, 381);
    }

    #[test]
    fn from_outputs_primary_is_first_output() {
        let outs = vec![
            RandrOutput {
                name: "A".into(),
                output_id: 1,
                crtc_id: 3,
                mode_id: 5,
                connected: true,
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
            RandrOutput {
                name: "B".into(),
                output_id: 2,
                crtc_id: 4,
                mode_id: 5,
                connected: true,
                x: 1024,
                y: 0,
                width: 1024,
                height: 768,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.primary_output, 1);
    }

    #[test]
    fn disconnected_output_is_still_queryable() {
        let mut outs = vec![RandrOutput {
            name: "DP-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
        }];
        outs.push(RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 4,
            crtc_id: 5,
            mode_id: 0,
            connected: false,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            vrefresh: 0,
            mm_width: 0,
            mm_height: 0,
        });
        let st = RandrState::from_outputs(1, outs);
        let info = st
            .output_info(4, 0)
            .expect("disconnected output still queryable");
        assert_eq!(info.crtc, 0);
        assert_eq!(info.connection, 1);
        assert_eq!(st.screen_width, 2560);
        assert_eq!(st.modes.len(), 1);
        assert_eq!(st.primary_output, 1);
    }

    #[test]
    fn primary_prefers_connected_when_lower_id_is_disconnected() {
        let outs = vec![
            RandrOutput {
                name: "DP-1".into(),
                output_id: 1,
                crtc_id: 2,
                mode_id: 0,
                connected: false,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                vrefresh: 0,
                mm_width: 0,
                mm_height: 0,
            },
            RandrOutput {
                name: "HDMI-A-1".into(),
                output_id: 4,
                crtc_id: 5,
                mode_id: 6,
                connected: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
            },
        ];
        assert_eq!(RandrState::from_outputs(1, outs).primary_output, 4);
    }
}
