//! RANDR user modes (`RRCreateMode` .. `RRDeleteOutputMode`) and the one
//! projection, [`RandrView`], that puts them next to the backend's modes for
//! every reply, event and check.
//!
//! The backend rebuilds `RandrState` on every set, probe and hotplug, so the
//! user modes live beside it in `ServerState::randr_user_modes`. Xorg keeps
//! them until the server resets (`rrmode.c:91-92`): they belong to the
//! server, not to the client that created them.

use std::collections::HashMap;

use yserver_protocol::x11::randr as proto;

use super::{
    CrtcInfoData, OutputInfoReplyData, RR_MODE_FLAG_BITS, RandrMode, RandrOutput, RandrState,
};
use crate::server::{RANDR_USER_MODE_XID_FIRST, RANDR_USER_MODE_XID_LAST};

/// One mode made by `RRCreateMode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMode {
    pub id: u32,
    pub name: Vec<u8>,
    /// The `xRRModeInfo` as the client sent it, `id` replaced by ours and
    /// `name_len` by the stored name's length. Every other field, `hskew`
    /// and all flag bits included, is echoed verbatim, as Xorg does.
    pub info: proto::ModeInfo,
}

/// The user modes and the per-output user lists (`output->userModes`).
#[derive(Debug, Clone)]
pub struct RandrUserModes {
    /// Creation order, which `GetScreenResources` reports.
    modes: Vec<UserMode>,
    /// Output id -> the modes added to it, in `AddOutputMode` order. Any
    /// mode may be added, not only user modes (`rroutput.c:199-235`).
    /// Kept across hotplug: KMS output ids are stable per connector, and
    /// Xorg keeps the list on a disconnected connector.
    on_output: HashMap<u32, Vec<u32>>,
    next_id: u32,
    first_id: u32,
    last_id: u32,
}

impl Default for RandrUserModes {
    fn default() -> Self {
        Self::with_id_range(RANDR_USER_MODE_XID_FIRST, RANDR_USER_MODE_XID_LAST)
    }
}

impl RandrUserModes {
    /// A store allocating ids from `first..=last`.
    #[must_use]
    pub(crate) fn with_id_range(first: u32, last: u32) -> Self {
        assert!(first != 0 && first <= last);
        Self {
            modes: Vec::new(),
            on_output: HashMap::new(),
            next_id: first,
            first_id: first,
            last_id: last,
        }
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&UserMode> {
        self.modes.iter().find(|mode| mode.id == id)
    }

    /// The user modes in creation order.
    #[must_use]
    pub fn modes(&self) -> &[UserMode] {
        &self.modes
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modes.is_empty() && self.on_output.is_empty()
    }

    /// The next free id, without taking it; `None` when every id in the
    /// range is in use (`BadAlloc`). Like Xorg's `FakeClientID`, freed ids
    /// are found again once the counter has gone round.
    #[must_use]
    pub fn next_free_id(&self) -> Option<u32> {
        let span = self.last_id - self.first_id + 1;
        if u32::try_from(self.modes.len()).is_ok_and(|n| n >= span) {
            return None;
        }
        let mut id = self.next_id;
        loop {
            if self.get(id).is_none() {
                return Some(id);
            }
            id = if id == self.last_id {
                self.first_id
            } else {
                id + 1
            };
        }
    }

    /// Store a mode under `id`, an id [`Self::next_free_id`] returned.
    pub fn insert(&mut self, id: u32, name: Vec<u8>, mut info: proto::ModeInfo) {
        debug_assert!(self.get(id).is_none());
        info.id = id;
        info.name_len = u16::try_from(name.len()).unwrap_or(u16::MAX);
        self.modes.push(UserMode { id, name, info });
        self.next_id = if id == self.last_id {
            self.first_id
        } else {
            id + 1
        };
    }

    /// Free a user mode. The caller has checked it is not referenced.
    pub fn remove(&mut self, id: u32) -> bool {
        let before = self.modes.len();
        self.modes.retain(|mode| mode.id != id);
        self.modes.len() != before
    }

    /// The modes added to `output_id`, in add order.
    #[must_use]
    pub fn output_list(&self, output_id: u32) -> &[u32] {
        self.on_output.get(&output_id).map_or(&[], Vec::as_slice)
    }

    /// Whether any output's user list holds `id`.
    #[must_use]
    pub fn on_any_output(&self, id: u32) -> bool {
        self.on_output.values().any(|list| list.contains(&id))
    }

    /// Append `id` to `output_id`'s user list.
    pub fn add_to_output(&mut self, output_id: u32, id: u32) {
        let list = self.on_output.entry(output_id).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
    }

    /// Remove `id` from `output_id`'s user list.
    pub fn remove_from_output(&mut self, output_id: u32, id: u32) -> bool {
        let Some(list) = self.on_output.get_mut(&output_id) else {
            return false;
        };
        let before = list.len();
        list.retain(|&entry| entry != id);
        let removed = list.len() != before;
        if list.is_empty() {
            self.on_output.remove(&output_id);
        }
        removed
    }
}

/// The name replies advertise for a backend mode.
fn backend_mode_name(mode: &RandrMode) -> Vec<u8> {
    format!("{}x{}", mode.width, mode.height).into_bytes()
}

fn backend_mode_info(mode: &RandrMode, name_len: u16) -> proto::ModeInfo {
    timing_info(
        mode.mode_id,
        mode.width,
        mode.height,
        mode.effective_timing(),
        name_len,
    )
}

// Shared with VidMode's GetModeLine — see `EffectiveTiming`.
fn timing_info(
    id: u32,
    width: u16,
    height: u16,
    t: super::EffectiveTiming,
    name_len: u16,
) -> proto::ModeInfo {
    proto::ModeInfo {
        id,
        width,
        height,
        dot_clock: t.dot_clock_hz,
        hsync_start: t.hsync_start,
        hsync_end: t.hsync_end,
        htotal: t.htotal,
        hskew: t.hskew,
        vsync_start: t.vsync_start,
        vsync_end: t.vsync_end,
        vtotal: t.vtotal,
        name_len,
        mode_flags: t.mode_flags,
    }
}

/// Xorg `xf86RandRModeMatches` (`xf86RandR12.c:996-1038`): whether the
/// RANDR mode `mode` has the programmed timing `current`. The clock in
/// whole kHz (`dotClock / 1000` against `Clock`, truncated), every other
/// timing field including `hSkew`, and the flags under `FLAG_BITS`. Names
/// and ids are not compared.
#[must_use]
pub fn mode_matches_timing(mode: &proto::ModeInfo, current: &proto::ModeInfo) -> bool {
    mode.dot_clock / 1000 == current.dot_clock / 1000
        && mode.width == current.width
        && mode.hsync_start == current.hsync_start
        && mode.hsync_end == current.hsync_end
        && mode.htotal == current.htotal
        && mode.hskew == current.hskew
        && mode.height == current.height
        && mode.vsync_start == current.vsync_start
        && mode.vsync_end == current.vsync_end
        && mode.vtotal == current.vtotal
        && (mode.mode_flags & RR_MODE_FLAG_BITS) == (current.mode_flags & RR_MODE_FLAG_BITS)
}

/// `RandrState` plus the user modes: what clients see.
#[derive(Clone, Copy)]
pub struct RandrView<'a> {
    pub randr: &'a RandrState,
    pub user: &'a RandrUserModes,
}

impl<'a> RandrView<'a> {
    #[must_use]
    pub fn new(randr: &'a RandrState, user: &'a RandrUserModes) -> Self {
        Self { randr, user }
    }

    /// A backend mode by id.
    #[must_use]
    pub fn backend_mode(&self, id: u32) -> Option<&'a RandrMode> {
        self.randr
            .mode_table
            .iter()
            .chain(self.randr.modes.iter())
            .find(|mode| mode.mode_id == id)
    }

    /// Whether `id` names a mode, backend or user. A backend id the
    /// current-mode match replaced by a user mode does not exist for
    /// clients: on Xorg there is no such mode.
    #[must_use]
    pub fn mode_exists(&self, id: u32) -> bool {
        id != 0
            && (self.user.get(id).is_some()
                || (self.backend_mode(id).is_some() && !self.backend_id_replaced(id)))
    }

    /// The `ModeInfo` of mode `id`, user or backend, without its name.
    #[must_use]
    pub fn mode_info(&self, id: u32) -> Option<proto::ModeInfo> {
        match self.user.get(id) {
            Some(user) => Some(user.info),
            None => self.backend_mode(id).map(|mode| backend_mode_info(mode, 0)),
        }
    }

    /// `id` is the backend's current mode of some CRTC that reports a user
    /// mode instead, and nothing else lists or reports it.
    fn backend_id_replaced(&self, id: u32) -> bool {
        let outputs = &self.randr.outputs;
        outputs
            .iter()
            .any(|o| o.mode_id == id && self.crtc_current_mode(o) != id)
            && !outputs.iter().any(|o| {
                o.mode_ids.contains(&id)
                    || self.user.output_list(o.output_id).contains(&id)
                    || self.crtc_current_mode(o) == id
            })
    }

    /// `output`'s user-list entries that still name a mode and are not in
    /// its backend list, in add order.
    fn output_user_ids(&self, output: &RandrOutput) -> impl Iterator<Item = u32> + '_ {
        let backend = output.mode_ids.clone();
        self.user
            .output_list(output.output_id)
            .iter()
            .copied()
            .filter(move |&id| !backend.contains(&id) && self.mode_info(id).is_some())
    }

    /// Whether `mode` is in `output_id`'s probed or user list.
    #[must_use]
    pub fn output_lists_mode(&self, output_id: u32, mode: u32) -> bool {
        self.randr
            .outputs
            .iter()
            .find(|o| o.output_id == output_id)
            .is_some_and(|o| {
                o.mode_ids.contains(&mode) || self.output_user_ids(o).any(|id| id == mode)
            })
    }

    /// The mode the CRTC of `output`'s row reports (`xf86RandR12CrtcNotify`,
    /// `xf86RandR12.c:1077-1088`): the first mode of the output's probed
    /// list, then of its user list, whose timing matches the programmed
    /// one ([`mode_matches_timing`]); the backend id when none does.
    #[must_use]
    pub fn crtc_current_mode(&self, output: &RandrOutput) -> u32 {
        if output.mode_id == 0 {
            return 0;
        }
        let current = timing_info(
            output.mode_id,
            output.width,
            output.height,
            output.effective_timing(),
            0,
        );
        output
            .mode_ids
            .iter()
            .copied()
            .chain(self.output_user_ids(output))
            .find(|&id| {
                self.mode_info(id)
                    .is_some_and(|info| mode_matches_timing(&info, &current))
            })
            .unwrap_or(output.mode_id)
    }

    /// Project a backend mode id a notification carries for `crtc_id`.
    #[must_use]
    pub fn project_crtc_mode(&self, crtc_id: u32, mode: u32) -> u32 {
        self.randr
            .outputs
            .iter()
            .find(|o| o.crtc_id == crtc_id && o.mode_id == mode)
            .map_or(mode, |o| self.crtc_current_mode(o))
    }

    /// Whether any CRTC currently reports `mode`.
    #[must_use]
    pub fn mode_is_current(&self, mode: u32) -> bool {
        mode != 0
            && self
                .randr
                .outputs
                .iter()
                .any(|o| self.crtc_current_mode(o) == mode)
    }

    /// The mode of `output_id`'s assigned CRTC (`output->crtc->mode`), 0
    /// without one.
    #[must_use]
    pub fn output_crtc_mode(&self, output_id: u32) -> u32 {
        let crtc = self.randr.assigned_crtc(output_id);
        if crtc == 0 {
            return 0;
        }
        self.randr
            .outputs
            .iter()
            .find(|o| o.crtc_id == crtc)
            .map_or(0, |o| self.crtc_current_mode(o))
    }

    /// Whether a mode by this name exists (`RRModeFindByName`): a user mode
    /// or a name the replies advertise for a backend mode.
    #[must_use]
    pub fn mode_name_in_use(&self, name: &[u8]) -> bool {
        self.user.modes().iter().any(|mode| mode.name == name)
            || self
                .randr
                .mode_table
                .iter()
                .chain(self.randr.modes.iter())
                .any(|mode| {
                    backend_mode_name(mode) == name && !self.backend_id_replaced(mode.mode_id)
                })
    }

    /// `RRModesForScreen` (`rrmode.c:161-227`), deduped: each output's
    /// probed then user modes, then each CRTC's current mode, then every
    /// remaining user mode in creation order. Backend modes no output or
    /// CRTC references stay advertised after those, in table order.
    fn screen_mode_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = Vec::new();
        let mut push = |id: u32| {
            if id != 0 && !ids.contains(&id) && self.mode_exists(id) {
                ids.push(id);
            }
        };
        for output in &self.randr.outputs {
            for &id in &output.mode_ids {
                push(id);
            }
            for id in self.output_user_ids(output) {
                push(id);
            }
        }
        for output in &self.randr.outputs {
            push(self.crtc_current_mode(output));
        }
        for mode in self.user.modes() {
            push(mode.id);
        }
        for mode in &self.randr.mode_table {
            push(mode.mode_id);
        }
        ids
    }

    /// `GetScreenResources(Current)`.
    #[must_use]
    pub fn screen_resources_current(&self) -> proto::ScreenResources {
        let crtcs: Vec<u32> = self.randr.outputs.iter().map(|o| o.crtc_id).collect();
        let outputs: Vec<u32> = self.randr.outputs.iter().map(|o| o.output_id).collect();
        let mut modes = Vec::new();
        let mut mode_names = Vec::new();
        for id in self.screen_mode_ids() {
            if let Some(user) = self.user.get(id) {
                modes.push(user.info);
                mode_names.extend_from_slice(&user.name);
            } else if let Some(mode) = self.backend_mode(id) {
                let name = backend_mode_name(mode);
                let name_len = u16::try_from(name.len()).unwrap_or(u16::MAX);
                modes.push(backend_mode_info(mode, name_len));
                mode_names.extend_from_slice(&name);
            }
        }
        proto::ScreenResources {
            timestamp: self.randr.timestamp,
            config_timestamp: self.randr.config_timestamp,
            crtcs,
            outputs,
            modes,
            mode_names,
        }
    }

    /// `GetOutputInfo` (`rroutput.c:523-560`): probed modes, then user
    /// modes; `nPreferred` counts probed modes only.
    #[must_use]
    pub fn output_info(
        &self,
        output_id: u32,
        config_timestamp: u32,
    ) -> Option<OutputInfoReplyData> {
        let mut info = self.randr.output_info(output_id, config_timestamp)?;
        let output = self
            .randr
            .outputs
            .iter()
            .find(|o| o.output_id == output_id)?;
        info.mode_ids.extend(self.output_user_ids(output));
        info.mode_id = self.crtc_current_mode(output);
        Some(info)
    }

    /// `GetCrtcInfo`.
    #[must_use]
    pub fn crtc_info(&self, crtc_id: u32, config_timestamp: u32) -> Option<CrtcInfoData> {
        let mut info = self.randr.crtc_info(crtc_id, config_timestamp)?;
        if let Some(output) = self.randr.outputs.iter().find(|o| o.crtc_id == crtc_id) {
            info.mode_id = self.crtc_current_mode(output);
        }
        Some(info)
    }

    /// A user mode as a `RandrMode`, for the size checks of
    /// `SetCrtcConfig`. Only width and height are meaningful.
    fn user_randr_mode(user: &UserMode) -> RandrMode {
        let dots = u64::from(user.info.htotal) * u64::from(user.info.vtotal);
        let vrefresh = (u64::from(user.info.dot_clock) + dots / 2)
            .checked_div(dots)
            .map_or(0, |r| u32::try_from(r).unwrap_or(u32::MAX));
        RandrMode {
            mode_id: user.id,
            width: user.info.width,
            height: user.info.height,
            vrefresh,
            timing: None,
        }
    }

    /// Validate `SetCrtcConfig` in yserver's order: arity, outputs, then mode.
    /// Xorg verifies the mode immediately after the CRTC and before outputs
    /// (`randr/rrcrtc.c:1294-1320`), so if both a mode and output are bad the
    /// client sees a different error here. Rotation is checked afterward by
    /// the handler. `check_resource` runs immediately after each output or
    /// mode resolves. The mode must be in the output's probed or user list
    /// (`rrcrtc.c:1339-1350`). `Ok(None)` = disable; `Ok(Some(mode))` =
    /// enable with this resolved mode; `Err((code, error_value))` = protocol
    /// error + field-specific `errorValue`.
    pub fn validate_set_crtc_config(
        &self,
        crtc_id: u32,
        mode_id: u32,
        outputs: &[u32],
        mut check_resource: impl FnMut(u32, crate::access::ResourceKind) -> Result<(), (u8, u32)>,
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
            let Some(out) = self.randr.outputs.iter().find(|o| o.output_id == oid) else {
                return Err((error::BAD_MATCH, oid)); // unknown output
            };
            check_resource(oid, crate::access::ResourceKind::RandrOutput)?;
            if out.crtc_id != crtc_id {
                return Err((error::BAD_MATCH, crtc_id)); // output doesn't drive crtc
            }
        }
        // The addressed crtc must belong to one of the named outputs.
        let out = self
            .randr
            .outputs
            .iter()
            .find(|o| o.crtc_id == crtc_id && outputs.contains(&o.output_id))
            .ok_or((error::BAD_MATCH, crtc_id))?;
        // 3. mode ∈ this output's probed or user list. errorValue = bad mode.
        if !self.output_lists_mode(out.output_id, mode_id) {
            return Err((error::BAD_MATCH, mode_id));
        }
        let mode = match self.user.get(mode_id) {
            Some(user) => Self::user_randr_mode(user),
            None => self
                .backend_mode(mode_id)
                .copied()
                .ok_or((error::BAD_MATCH, mode_id))?,
        };
        check_resource(mode_id, crate::access::ResourceKind::RandrMode)?;
        Ok(Some(mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: u16, height: u16) -> proto::ModeInfo {
        proto::ModeInfo {
            id: 0xdead,
            width,
            height,
            dot_clock: 173_000_000,
            hsync_start: 2048,
            hsync_end: 2248,
            htotal: 2576,
            hskew: 0,
            vsync_start: 1083,
            vsync_end: 1088,
            vtotal: 1120,
            name_len: 99,
            mode_flags: 0x6,
        }
    }

    #[test]
    fn ids_come_from_the_user_mode_range_and_replace_the_request_id() {
        let mut store = RandrUserModes::default();
        let id = store.next_free_id().unwrap();
        assert_eq!(id, RANDR_USER_MODE_XID_FIRST);
        store.insert(id, b"foo".to_vec(), info(1920, 1080));
        let mode = store.get(id).unwrap();
        assert_eq!((mode.info.id, mode.info.name_len), (id, 3));
        assert_eq!(store.next_free_id(), Some(id + 1));
    }

    #[test]
    fn exhausted_range_has_no_id_and_freed_ids_come_back() {
        let mut store = RandrUserModes::with_id_range(0x10, 0x11);
        for name in [b"a", b"b"] {
            let id = store.next_free_id().unwrap();
            store.insert(id, name.to_vec(), info(640, 480));
        }
        assert_eq!(store.next_free_id(), None);
        assert!(store.remove(0x10));
        assert_eq!(store.next_free_id(), Some(0x10));
    }

    #[test]
    fn output_lists_keep_add_order_and_drop_empty_entries() {
        let mut store = RandrUserModes::default();
        store.add_to_output(7, 3);
        store.add_to_output(7, 1);
        store.add_to_output(7, 3);
        assert_eq!(store.output_list(7), &[3, 1]);
        assert!(store.on_any_output(1));
        assert!(store.remove_from_output(7, 3));
        assert!(!store.remove_from_output(7, 3));
        assert!(store.remove_from_output(7, 1));
        assert!(store.is_empty());
    }
}
