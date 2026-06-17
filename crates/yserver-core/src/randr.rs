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
    /// Available mode ids for this output, preferred-first. Empty for a
    /// disconnected output. The current mode is `mode_id` (0 = off).
    pub mode_ids: Vec<u32>,
    /// Count of leading entries in `mode_ids` that are preferred modes
    /// (Xorg `GetOutputInfo` `nPreferred`).
    pub num_preferred: u16,
}

/// One unique mode (deduped by `(width, height, vrefresh)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Full deduped advertised mode union for `GetScreenResources`.
    pub mode_table: Vec<RandrMode>,
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
    /// Aggregation (boot default; `RRSetScreenSize` later overrides the
    /// reported `screen_width`/`screen_height`):
    /// - `screen_width = max(output.x + output.width)`
    /// - `screen_height = max(output.y + output.height)` (2-D: a CRTC
    ///   may sit at any `(x, y)`, e.g. a monitor stacked below)
    /// - `*_mm` derived from screen_* at 96 DPI
    /// - `primary_output = outputs[0].output_id` (0 if empty)
    #[must_use]
    pub fn from_outputs(timestamp: u32, outputs: Vec<RandrOutput>) -> Self {
        let mode_table = Self::current_mode_table(&outputs);
        Self::from_outputs_with_modes(timestamp, outputs, mode_table)
    }

    /// Build a `RandrState` from pre-allocated outputs plus an explicit
    /// deduped mode table. The caller owns the table shape; `modes`
    /// remains the current-mode-only compatibility vector used by a few
    /// legacy call sites.
    #[must_use]
    pub fn from_outputs_with_modes(
        timestamp: u32,
        outputs: Vec<RandrOutput>,
        mode_table: Vec<RandrMode>,
    ) -> Self {
        // Some compositors compare the first RANDR resource timestamp with
        // their own "last SetCrtcConfig" timestamp, which starts at zero.
        // Advertising zero here makes the initial server state look like a
        // completed client-side reconfiguration.
        let timestamp = timestamp.max(1);
        let modes = Self::current_mode_table(&outputs);
        let screen_width: u16 = outputs
            .iter()
            .map(|o| {
                let r = i32::from(o.x).saturating_add(i32::from(o.width));
                u16::try_from(r.max(0)).unwrap_or(u16::MAX)
            })
            .max()
            .unwrap_or(0);
        let screen_height: u16 = outputs
            .iter()
            .map(|o| {
                let r = i32::from(o.y).saturating_add(i32::from(o.height));
                u16::try_from(r.max(0)).unwrap_or(u16::MAX)
            })
            .max()
            .unwrap_or(0);
        // mm = px * 25.4 / 96; integer form: (px*254 + 480) / 960. Previous
        // divisor was off by 10× and made GTK auto-scale at extreme factors.
        let width_mm = ((u32::from(screen_width) * 254 + 480) / 960).max(1);
        let height_mm = ((u32::from(screen_height) * 254 + 480) / 960).max(1);

        let primary_output = outputs
            .iter()
            .find(|o| o.connected && o.mode_id != 0)
            .or_else(|| outputs.iter().find(|o| o.connected))
            .or_else(|| outputs.first())
            .map_or(0, |o| o.output_id);

        Self {
            timestamp,
            config_timestamp: timestamp,
            outputs,
            modes,
            mode_table,
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
            mode_ids: vec![3],
            num_preferred: 1,
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

    /// Would shrinking the logical screen to `w`×`h` crop any enabled
    /// output? (Xorg `RRSetScreenSize` BadMatch, rrscreen.c:266.)
    #[must_use]
    pub fn screen_size_would_crop(&self, w: u16, h: u16) -> bool {
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode_id != 0)
            .any(|o| {
                i32::from(o.x) + i32::from(o.width) > i32::from(w)
                    || i32::from(o.y) + i32::from(o.height) > i32::from(h)
            })
    }

    /// Set the logical (reported) screen size after validation. Uses
    /// the CLIENT-supplied physical mm verbatim (Xorg `RRScreenSizeSet`
    /// passes `stuff->widthInMillimeters`/`heightInMillimeters` — it does
    /// NOT recompute from pixels). Does not touch outputs.
    pub fn set_logical_size(&mut self, timestamp: u32, w: u16, h: u16, mm_w: u32, mm_h: u32) {
        self.screen_width = w;
        self.screen_height = h;
        self.width_mm = mm_w;
        self.height_mm = mm_h;
        self.timestamp = timestamp.max(1);
        self.config_timestamp = self.timestamp;
    }

    /// Monitors for RANDR `GetMonitors` / XINERAMA: one per ENABLED
    /// output (connected with a non-zero mode), at its `(x,y,w,h)`.
    /// Off and disconnected outputs are absent (Xorg builds an
    /// automatic monitor only for an output with an active CRTC).
    pub fn enabled_outputs(&self) -> impl Iterator<Item = &RandrOutput> {
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode_id != 0)
    }

    /// Build a `ScreenResources` reply describing every output / CRTC /
    /// mode currently configured.
    #[must_use]
    pub fn screen_resources_current(&self) -> proto::ScreenResources {
        let crtcs: Vec<u32> = self.outputs.iter().map(|o| o.crtc_id).collect();
        let outputs: Vec<u32> = self.outputs.iter().map(|o| o.output_id).collect();

        let mut mode_names: Vec<u8> = Vec::new();
        let mut mode_infos: Vec<proto::ModeInfo> = Vec::with_capacity(self.mode_table.len());
        for m in &self.mode_table {
            let name = format!("{}x{}", m.width, m.height).into_bytes();
            #[allow(clippy::cast_possible_truncation)]
            let name_len = name.len() as u16;
            // Synthetic blanking. dot_clock MUST be consistent with the
            // htotal/vtotal we emit: clients (xrandr, mate-settings)
            // compute refresh as dot_clock / (htotal * vtotal), so
            // setting dot_clock = htotal * vtotal * vrefresh makes that
            // back-compute to exactly `vrefresh`. (Using width*height
            // instead reported vrefresh * active/total ≈ 51–53 Hz.)
            let htotal = m.width.saturating_add(264);
            let vtotal = m.height.saturating_add(28);
            let dot_clock = u32::from(htotal) * u32::from(vtotal) * m.vrefresh;
            mode_infos.push(proto::ModeInfo {
                id: m.mode_id,
                width: m.width,
                height: m.height,
                dot_clock,
                hsync_start: m.width + 40,
                hsync_end: m.width + 168,
                htotal,
                hskew: 0,
                vsync_start: m.height + 1,
                vsync_end: m.height + 4,
                vtotal,
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
        // A connected-but-OFF output (`mode_id == 0`) has no active mode,
        // so `width`/`height` are 0. Only the EDID-reported physical size
        // is meaningful then — the 96-DPI synthesis from pixel dimensions
        // would fabricate a 1mm×1mm size (→ nonsense DPI for a client that
        // queries GetOutputInfo before enabling the output). Synthesize
        // only when an active mode gives real pixel dimensions; otherwise
        // report the EDID size if present, else 0 (unknown).
        let enabled = out.connected && out.mode_id != 0;
        let synth_mm = |px: u16| ((u32::from(px) * 254 + 480) / 960).max(1);
        let width_mm = if out.mm_width > 0 {
            out.mm_width
        } else if enabled {
            synth_mm(out.width)
        } else {
            0
        };
        let height_mm = if out.mm_height > 0 {
            out.mm_height
        } else if enabled {
            synth_mm(out.height)
        } else {
            0
        };
        Some(OutputInfoReplyData {
            timestamp: self.timestamp,
            // Currently-assigned CRTC: 0 (unassigned) unless the output is
            // actually enabled. A connected-but-off output reports crtc=0.
            crtc: if enabled { out.crtc_id } else { 0 },
            // The set of CRTCs this output *can* be driven by (Xorg
            // `crtcs`), independent of whether one is currently assigned.
            // Our model is a stable 1:1 output↔crtc allocation, so the
            // possible list is always this output's own crtc id.
            possible_crtcs: vec![out.crtc_id],
            mode_id: out.mode_id,
            width_mm,
            height_mm,
            name: out.name.clone(),
            connection: if out.connected { 0 } else { 1 },
            mode_ids: out.mode_ids.clone(),
            num_preferred: out.num_preferred,
        })
    }

    /// Validate arity + output/mode resolution for `SetCrtcConfig` (Xorg
    /// `rrcrtc.c` order, EXCLUDING rotation + bounds — the handler does
    /// those after, in that order). `Ok(None)` = disable;
    /// `Ok(Some(mode))` = enable with this resolved mode;
    /// `Err((code, error_value))` = protocol error + field-specific
    /// `errorValue`.
    pub fn validate_set_crtc_config(
        &self,
        crtc_id: u32,
        mode_id: u32,
        outputs: &[u32],
    ) -> Result<Option<RandrMode>, (u8, u32)> {
        use yserver_protocol::x11::error;
        // 1. mode/outputs arity. errorValue = the addressed crtc.
        if mode_id == 0 {
            if !outputs.is_empty() {
                return Err((error::BAD_MATCH, crtc_id));
            }
            return Ok(None); // disable
        }
        if outputs.is_empty() {
            return Err((error::BAD_MATCH, crtc_id));
        }
        // 2. outputs resolve to known connectors AND drive this crtc (1:1).
        for &oid in outputs {
            let Some(out) = self.outputs.iter().find(|o| o.output_id == oid) else {
                return Err((error::BAD_MATCH, oid)); // unknown output
            };
            if out.crtc_id != crtc_id {
                return Err((error::BAD_MATCH, crtc_id)); // output doesn't drive crtc
            }
        }
        // The addressed crtc must belong to one of the named outputs.
        let out = self
            .outputs
            .iter()
            .find(|o| o.crtc_id == crtc_id && outputs.contains(&o.output_id))
            .ok_or((error::BAD_MATCH, crtc_id))?;
        // 3. mode ∈ this output's advertised list. errorValue = bad mode.
        if !out.mode_ids.contains(&mode_id) {
            return Err((error::BAD_MATCH, mode_id));
        }
        let mode = self
            .mode_table
            .iter()
            .find(|m| m.mode_id == mode_id)
            .copied()
            .ok_or((error::BAD_MATCH, mode_id))?;
        Ok(Some(mode))
    }

    /// Bounds check: does `mode` placed at `(x, y)` fit the current
    /// (logical) screen? Xorg `rrcrtc.c`: `x + width > screen.width` ⇒
    /// `BadValue(errorValue=x)`; then `y + height > screen.height` ⇒
    /// `BadValue(errorValue=y)`.
    pub fn screen_encompasses(&self, mode: &RandrMode, x: i16, y: i16) -> Result<(), (u8, u32)> {
        use yserver_protocol::x11::error;
        // Xorg rrcrtc.c: `x + width > screen.width` ⇒ BadValue(x), then
        // `y + height > screen.height` ⇒ BadValue(y). errorValue carries
        // the raw INT16 sign-extended into the CARD32 field.
        if i32::from(x) + i32::from(mode.width) > i32::from(self.screen_width) {
            return Err((error::BAD_VALUE, i32::from(x) as u32));
        }
        if i32::from(y) + i32::from(mode.height) > i32::from(self.screen_height) {
            return Err((error::BAD_VALUE, i32::from(y) as u32));
        }
        Ok(())
    }

    fn current_mode_table(outputs: &[RandrOutput]) -> Vec<RandrMode> {
        let mut modes: Vec<RandrMode> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        // Skip connected-but-OFF outputs: their `mode_id` is 0, which is
        // reserved for `None` and must never appear in the mode table.
        for out in outputs.iter().filter(|o| o.connected && o.mode_id != 0) {
            if seen.insert(out.mode_id) {
                modes.push(RandrMode {
                    mode_id: out.mode_id,
                    width: out.width,
                    height: out.height,
                    vrefresh: out.vrefresh,
                });
            }
        }
        modes
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
    /// Currently-assigned CRTC (0 = unassigned / output off).
    pub crtc: u32,
    /// CRTCs this output can be driven by (the RANDR `crtcs` array — the
    /// *possible* set, not the assigned one).
    pub possible_crtcs: Vec<u32>,
    pub mode_id: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub name: String,
    pub connection: u8,
    pub mode_ids: Vec<u32>,
    pub num_preferred: u16,
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
    fn output_info_reports_full_mode_list_preferred_first() {
        let outs = vec![RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 7,
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![7, 8, 9],
            num_preferred: 2,
        }];
        let st = RandrState::from_outputs(0, outs);
        let info = st.output_info(1, 0).expect("output 1");
        assert_eq!(info.mode_ids, vec![7, 8, 9]);
        assert_eq!(info.num_preferred, 2);
        assert_eq!(info.mode_id, 7);
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
                mode_ids: vec![5],
                num_preferred: 1,
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
                mode_ids: vec![6],
                num_preferred: 1,
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
    fn screen_resources_emits_full_deduped_mode_union() {
        let outs = vec![RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 7,
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![7, 8],
            num_preferred: 1,
        }];
        let mode_table = vec![
            RandrMode {
                mode_id: 7,
                width: 1920,
                height: 1080,
                vrefresh: 60,
            },
            RandrMode {
                mode_id: 8,
                width: 1280,
                height: 720,
                vrefresh: 60,
            },
        ];
        let st = RandrState::from_outputs_with_modes(0, outs, mode_table);
        let res = st.screen_resources_current();
        assert_eq!(res.modes.len(), 2);
        assert!(res.modes.iter().any(|m| m.id == 8 && m.width == 1280));
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
                mode_ids: vec![5],
                num_preferred: 1,
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
                mode_ids: vec![5],
                num_preferred: 1,
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
                mode_ids: vec![5],
                num_preferred: 1,
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
                mode_ids: vec![6],
                num_preferred: 1,
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
            mode_ids: vec![3],
            num_preferred: 1,
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
            mode_ids: vec![3],
            num_preferred: 1,
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
                mode_ids: vec![5],
                num_preferred: 1,
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
                mode_ids: vec![5],
                num_preferred: 1,
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
            mode_ids: vec![3],
            num_preferred: 1,
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
            mode_ids: vec![],
            num_preferred: 0,
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
                mode_ids: vec![],
                num_preferred: 0,
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
                mode_ids: vec![6],
                num_preferred: 1,
            },
        ];
        assert_eq!(RandrState::from_outputs(1, outs).primary_output, 4);
    }

    #[test]
    fn from_outputs_screen_height_is_2d_for_vertical_stack() {
        // External monitor stacked BELOW the laptop panel: second
        // output at y=1080. screen_height must be 1080+1080=2160, not
        // max(height)=1080. (Spec: screen_height = max(y+height).)
        let outs = vec![
            RandrOutput {
                name: "eDP-1".into(),
                output_id: 1,
                crtc_id: 2,
                mode_id: 3,
                connected: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![3],
                num_preferred: 1,
            },
            RandrOutput {
                name: "HDMI-A-1".into(),
                output_id: 4,
                crtc_id: 5,
                mode_id: 6,
                connected: true,
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![6],
                num_preferred: 1,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.screen_width, 1920);
        assert_eq!(st.screen_height, 2160, "screen must encompass y+height");
    }

    #[test]
    fn connected_off_output_reports_unassigned_crtc_and_no_synth_mm() {
        // A hotplugged-but-not-yet-enabled output (connected, mode_id=0,
        // 0×0 geometry, no EDID). GetOutputInfo must report it as
        // RR_Connected but UNASSIGNED: crtc=0, no fabricated mm size, and
        // its possible-CRTCs list still advertises the stable crtc it can
        // drive. The mode table must not pick up the reserved mode id 0.
        let outs = vec![RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 10,
            crtc_id: 11,
            mode_id: 0,
            connected: true,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            vrefresh: 0,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![12, 13],
            num_preferred: 1,
        }];
        let st = RandrState::from_outputs(0, outs);

        // current_mode_table (via from_outputs) must skip mode_id 0.
        assert!(
            !st.modes.iter().any(|m| m.mode_id == 0),
            "reserved mode id 0 must never appear in the mode table",
        );

        let info = st.output_info(10, 0).expect("output present");
        assert_eq!(info.connection, 0, "output is RR_Connected");
        assert_eq!(info.crtc, 0, "off output has no assigned crtc");
        assert_eq!(
            info.possible_crtcs,
            vec![11],
            "possible-CRTCs still advertises the output's stable crtc",
        );
        assert_eq!(info.width_mm, 0, "no synthesized mm for an off output");
        assert_eq!(info.height_mm, 0);
        assert_eq!(info.mode_id, 0);

        // crtc_info on the (unassigned) crtc reports the off geometry.
        assert_eq!(st.crtc_info(11, 0).expect("crtc present").mode_id, 0);
    }

    #[test]
    fn from_outputs_preserves_nonzero_y_position() {
        // Task 5.1 layout-preservation contract (core half): the core
        // RANDR state faithfully reflects whatever (x, y) the backend
        // bridge supplies — it must NOT flatten a client-configured
        // non-zero position. (The backend recompact now skips
        // client-configured outputs; this guards the core side that
        // would otherwise mask a regression.)
        let outs = vec![RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 1080,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![3],
            num_preferred: 1,
        }];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.outputs[0].y, 1080, "y must not be flattened");
        assert_eq!(
            st.crtc_info(2, 0).expect("crtc present").y,
            1080,
            "crtc_info must report the preserved y",
        );
    }

    #[test]
    fn mode_info_refresh_backcomputes_to_vrefresh() {
        // Clients derive refresh = dot_clock / (htotal * vtotal). For a
        // 1920x1080@60 mode that division must yield exactly 60, not the
        // ~51 Hz that width*height*vrefresh produced.
        let out = RandrOutput {
            name: "DP-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 7,
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![7],
            num_preferred: 1,
        };
        let mode_table = vec![RandrMode {
            mode_id: 7,
            width: 1920,
            height: 1080,
            vrefresh: 60,
        }];
        let st = RandrState::from_outputs_with_modes(0, vec![out], mode_table);
        let res = st.screen_resources_current();
        let mi = res.modes.iter().find(|m| m.id == 7).expect("mode 7");
        let refresh = mi.dot_clock / (u32::from(mi.htotal) * u32::from(mi.vtotal));
        assert_eq!(refresh, 60, "xrandr-formula refresh must equal vrefresh");
    }

    #[test]
    fn enabled_outputs_excludes_off_and_disconnected() {
        let outs = vec![
            RandrOutput {
                // enabled
                name: "eDP-1".into(),
                output_id: 1,
                crtc_id: 2,
                mode_id: 3,
                connected: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![3],
                num_preferred: 1,
            },
            RandrOutput {
                // connected but OFF (mode_id 0)
                name: "HDMI-A-1".into(),
                output_id: 4,
                crtc_id: 5,
                mode_id: 0,
                connected: true,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                vrefresh: 0,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![6],
                num_preferred: 1,
            },
            RandrOutput {
                // disconnected
                name: "DP-2".into(),
                output_id: 7,
                crtc_id: 8,
                mode_id: 0,
                connected: false,
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                vrefresh: 0,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![],
                num_preferred: 0,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        let names: Vec<&str> = st.enabled_outputs().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["eDP-1"]);
        // The off output (output_id 4) must not be primary; the enabled
        // one (output_id 1) is.
        assert_eq!(st.primary_output, 1);
    }

    #[test]
    fn set_screen_size_rejects_crop_of_enabled_output() {
        // eDP at (0,0) 1920x1080 enabled. Shrinking the screen to
        // 1280x720 would crop it → BadMatch (caller maps Err to the
        // protocol error).
        let outs = vec![RandrOutput {
            name: "eDP-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 3,
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![3],
            num_preferred: 1,
        }];
        let st = RandrState::from_outputs(0, outs);
        assert!(
            st.screen_size_would_crop(1280, 720),
            "1280x720 crops 1920x1080"
        );
        assert!(
            !st.screen_size_would_crop(2560, 1440),
            "larger does not crop"
        );
    }

    #[test]
    fn set_logical_size_stores_client_mm_verbatim() {
        let mut st = RandrState::nested(1, 1920, 1080);
        // Caller passes client-supplied mm values; the impl must NOT
        // recompute them from pixels.
        st.set_logical_size(42, 2560, 1440, 597, 336);
        assert_eq!(st.screen_width, 2560);
        assert_eq!(st.screen_height, 1440);
        assert_eq!(st.width_mm, 597, "mm verbatim from client");
        assert_eq!(st.height_mm, 336, "mm verbatim from client");
        assert_eq!(st.timestamp, 42);
        assert_eq!(st.config_timestamp, 42);
    }

    use yserver_protocol::x11::error as x11_err;

    fn one_output_state() -> RandrState {
        RandrState::from_outputs_with_modes(
            1,
            vec![RandrOutput {
                name: "eDP-1".into(),
                output_id: 1,
                crtc_id: 2,
                mode_id: 7,
                connected: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![7, 8],
                num_preferred: 1,
            }],
            vec![
                RandrMode {
                    mode_id: 7,
                    width: 1920,
                    height: 1080,
                    vrefresh: 60,
                },
                RandrMode {
                    mode_id: 8,
                    width: 1280,
                    height: 720,
                    vrefresh: 60,
                },
            ],
        )
    }

    #[test]
    fn set_crtc_config_mode_none_with_outputs_is_badmatch() {
        let st = one_output_state();
        assert_eq!(
            st.validate_set_crtc_config(2, 0, &[1]),
            Err((x11_err::BAD_MATCH, 2))
        );
    }

    #[test]
    fn set_crtc_config_mode_set_with_no_outputs_is_badmatch() {
        let st = one_output_state();
        assert_eq!(
            st.validate_set_crtc_config(2, 7, &[]),
            Err((x11_err::BAD_MATCH, 2))
        );
    }

    #[test]
    fn set_crtc_config_output_not_driving_crtc_is_badmatch() {
        let st = one_output_state();
        // crtc 999 isn't this output's crtc → errorValue = the bad crtc.
        assert_eq!(
            st.validate_set_crtc_config(999, 7, &[1]),
            Err((x11_err::BAD_MATCH, 999))
        );
    }

    #[test]
    fn set_crtc_config_mode_not_in_output_list_is_badmatch() {
        let st = one_output_state();
        // bad mode → errorValue = the bad mode id.
        assert_eq!(
            st.validate_set_crtc_config(2, 555, &[1]),
            Err((x11_err::BAD_MATCH, 555))
        );
    }

    #[test]
    fn screen_encompasses_rejects_overflow_x_then_y() {
        let st = one_output_state(); // screen 1920x1080
        let m1080 = RandrMode {
            mode_id: 7,
            width: 1920,
            height: 1080,
            vrefresh: 60,
        };
        // Place 1920x1080 at x=100 → 2020 > 1920. errorValue = x.
        assert_eq!(
            st.screen_encompasses(&m1080, 100, 0),
            Err((x11_err::BAD_VALUE, 100))
        );
        // x ok, y overflow: at y=100 → 1180 > 1080. errorValue = y.
        assert_eq!(
            st.screen_encompasses(&m1080, 0, 100),
            Err((x11_err::BAD_VALUE, 100))
        );
        // exact fit (x+w == screen.width) is allowed (Xorg uses `>`).
        assert_eq!(st.screen_encompasses(&m1080, 0, 0), Ok(()));
    }

    #[test]
    fn set_crtc_config_valid_enable_resolves_mode() {
        let st = one_output_state();
        let mode = st
            .validate_set_crtc_config(2, 8, &[1])
            .expect("valid")
            .expect("enable");
        assert_eq!((mode.width, mode.height), (1280, 720));
    }

    #[test]
    fn set_crtc_config_valid_disable() {
        let st = one_output_state();
        assert_eq!(st.validate_set_crtc_config(2, 0, &[]), Ok(None));
    }
}
