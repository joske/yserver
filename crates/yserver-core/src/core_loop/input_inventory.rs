//! `InputInventory`: the process-lifetime record of every input device
//! libinput currently reports, kept beside the core loop rather than in
//! `ServerState`. Step 1 of
//! `docs/superpowers/plans/2026-09-09-server-reset-plan.md` — the seed
//! source a future reset (steps 4/5) will use to repopulate XI state
//! without a re-probe, since `backend.probe_input_devices` is a one-shot,
//! start-of-process no-op in Direct mode. See
//! `docs/superpowers/specs/2026-09-09-server-reset-design.md`,
//! "`InputInventory` — the seed source for input, and who owns it".
//!
//! Populated from process-lifetime `Message::HostInput` lifecycle events.
//!
//! Deliberately atom-free: `DeviceInfo` carries device facts only — no
//! interned property atoms, no XI device ids. That is what will let a
//! future generation seed from this inventory even though it destroys
//! the atom table wholesale; the moment an atom-bearing field lands
//! here, that guarantee is gone.

use std::collections::HashMap;

use super::message::DeviceInfo;
use crate::xinput::{InputSourceId, libinput_props::DeviceConfigChange};

/// Process-lifetime input device inventory, owned beside the core loop
/// (a local binding in `run_core`, never a field of `ServerState`).
#[derive(Debug, Default)]
pub struct InputInventory {
    devices: HashMap<InputSourceId, DeviceInfo>,
}

impl InputInventory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record or refresh the facts for one runtime source.
    pub fn add(&mut self, info: DeviceInfo) {
        self.devices.insert(info.source_id, info);
    }

    /// Mark a source disabled while preserving its facts for server reset.
    pub fn suspend(&mut self, source_id: InputSourceId) {
        if let Some(info) = self.devices.get_mut(&source_id) {
            info.enabled = false;
        }
    }

    /// Mark every known source unavailable in the same core-loop dispatch as
    /// VT release. The backend performs synchronous held-state cleanup before
    /// the VT handoff; later per-device suspend messages remain idempotent.
    pub fn suspend_all(&mut self) {
        for info in self.devices.values_mut() {
            info.enabled = false;
        }
    }

    /// Refresh a continued source after libinput has restored its settings.
    pub fn resume(&mut self, mut info: DeviceInfo) {
        info.enabled = true;
        self.add(info);
    }

    /// Record a configuration value only after the source's live libinput
    /// handle confirms it. Returns false when that source has already been
    /// removed from the process-lifetime inventory.
    pub fn update_config(&mut self, source: InputSourceId, change: DeviceConfigChange) -> bool {
        let Some(info) = self.devices.get_mut(&source) else {
            return false;
        };
        info.config.apply_confirmed(change);
        true
    }

    /// Drop one source. No-op if it is already absent.
    pub fn remove(&mut self, source_id: InputSourceId) {
        self.devices.remove(&source_id);
    }

    #[must_use]
    pub fn get(&self, source_id: InputSourceId) -> Option<&DeviceInfo> {
        self.devices.get(&source_id)
    }

    /// Every recorded source, ordered by runtime identity.
    #[must_use]
    pub fn devices_by_source(&self) -> Vec<&DeviceInfo> {
        let mut sources: Vec<InputSourceId> = self.devices.keys().copied().collect();
        sources.sort_unstable_by_key(|source_id| source_id.0);
        sources
            .into_iter()
            .filter_map(|source_id| self.devices.get(&source_id))
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_loop::message::LibinputConfigSnapshot;

    fn device(source: u64, node: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            source_id: crate::xinput::InputSourceId(source),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: name.into(),
            device_node: node.into(),
            sysname: node.trim_start_matches("/dev/input/").into(),
            vendor_id: 0x046d,
            product_id: 0x1234,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }
    }

    #[test]
    fn new_inventory_is_empty() {
        let inventory = InputInventory::new();
        assert!(inventory.is_empty());
        assert_eq!(inventory.len(), 0);
    }

    #[test]
    fn add_then_remove_leaves_the_expected_set() {
        let mut inventory = InputInventory::new();
        inventory.add(device(3, "/dev/input/event3", "Mouse"));
        inventory.add(device(4, "/dev/input/event4", "Touchpad"));
        assert_eq!(inventory.len(), 2);
        assert!(inventory.get(InputSourceId(3)).is_some());
        assert!(inventory.get(InputSourceId(4)).is_some());

        inventory.remove(InputSourceId(3));
        assert_eq!(inventory.len(), 1);
        assert!(inventory.get(InputSourceId(3)).is_none());
        assert!(inventory.get(InputSourceId(4)).is_some());
    }

    #[test]
    fn removing_an_absent_node_is_a_no_op() {
        let mut inventory = InputInventory::new();
        inventory.add(device(3, "/dev/input/event3", "Mouse"));
        inventory.remove(InputSourceId(9));
        assert_eq!(inventory.len(), 1);
    }

    #[test]
    fn suspend_all_marks_every_inventory_source_unavailable() {
        let mut inventory = InputInventory::new();
        inventory.add(device(3, "/dev/input/event3", "Mouse"));
        inventory.add(device(4, "/dev/input/event4", "Keyboard"));

        inventory.suspend_all();

        assert_eq!(inventory.len(), 2);
        assert_eq!(inventory.devices_by_source().len(), 2);
        assert!(
            inventory
                .devices_by_source()
                .iter()
                .all(|info| !info.enabled)
        );
    }

    #[test]
    fn duplicate_device_added_for_one_source_replaces_rather_than_duplicates() {
        let mut inventory = InputInventory::new();
        inventory.add(device(4, "/dev/input/event4", "Touchpad v1"));
        inventory.add(device(4, "/dev/input/event9", "Touchpad v2"));
        assert_eq!(
            inventory.len(),
            1,
            "a second DeviceAdded for the same source must replace, not duplicate"
        );
        assert_eq!(inventory.get(InputSourceId(4)).unwrap().name, "Touchpad v2");
    }
}
