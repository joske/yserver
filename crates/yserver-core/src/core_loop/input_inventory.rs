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
//! Populated purely from `Message::HostInput`'s `DeviceAdded`/
//! `DeviceRemoved` (see `crates/yserver/src/input/context.rs:314,331` for
//! the event shapes). Nothing consumes it yet — this step is additive
//! only, and does not change any existing behaviour.
//!
//! Deliberately atom-free: `DeviceInfo` carries device facts only — no
//! interned property atoms, no XI device ids. That is what will let a
//! future generation seed from this inventory even though it destroys
//! the atom table wholesale; the moment an atom-bearing field lands
//! here, that guarantee is gone.

use std::collections::HashMap;

use super::message::DeviceInfo;

/// Evdev device-node key (e.g. `/dev/input/event4`), as reported at
/// `DeviceAdded` time and used again verbatim at `DeviceRemoved`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceNode(String);

impl DeviceNode {
    #[must_use]
    pub fn new(device_node: impl Into<String>) -> Self {
        Self(device_node.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DeviceNode {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for DeviceNode {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Process-lifetime input device inventory, owned beside the core loop
/// (a local binding in `run_core`, never a field of `ServerState`).
#[derive(Debug, Default)]
pub struct InputInventory {
    devices: HashMap<DeviceNode, DeviceInfo>,
}

impl InputInventory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or re-record) a device. A duplicate `DeviceAdded` for a
    /// node already present replaces the entry rather than duplicating
    /// it — plain `HashMap::insert` semantics, called out because it is
    /// this step's stated proof obligation.
    pub fn add(&mut self, info: DeviceInfo) {
        let node = DeviceNode::new(info.device_node.clone());
        self.devices.insert(node, info);
    }

    /// Drop a device by node. No-op if the node isn't present.
    pub fn remove(&mut self, device_node: &str) {
        self.devices.remove(&DeviceNode::new(device_node));
    }

    #[must_use]
    pub fn get(&self, device_node: &str) -> Option<&DeviceInfo> {
        self.devices.get(&DeviceNode::new(device_node))
    }

    /// Every recorded device, ordered by device node.
    ///
    /// Sorted rather than in `HashMap` order because the consumer —
    /// the server-reset boundary — replays these into
    /// `ServerState::xi_seed_touchpad`, which writes a single
    /// latest-wins slave-pointer slot. Iteration order therefore
    /// decides which device wins, and a reset must not produce a
    /// different XI model each time it runs.
    #[must_use]
    pub fn devices_by_node(&self) -> Vec<&DeviceInfo> {
        let mut nodes: Vec<&DeviceNode> = self.devices.keys().collect();
        nodes.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        nodes
            .into_iter()
            .filter_map(|node| self.devices.get(node))
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

    fn device(node: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
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
        inventory.add(device("/dev/input/event3", "Mouse"));
        inventory.add(device("/dev/input/event4", "Touchpad"));
        assert_eq!(inventory.len(), 2);
        assert!(inventory.get("/dev/input/event3").is_some());
        assert!(inventory.get("/dev/input/event4").is_some());

        inventory.remove("/dev/input/event3");
        assert_eq!(inventory.len(), 1);
        assert!(inventory.get("/dev/input/event3").is_none());
        assert!(inventory.get("/dev/input/event4").is_some());
    }

    #[test]
    fn removing_an_absent_node_is_a_no_op() {
        let mut inventory = InputInventory::new();
        inventory.add(device("/dev/input/event3", "Mouse"));
        inventory.remove("/dev/input/event9");
        assert_eq!(inventory.len(), 1);
    }

    #[test]
    fn duplicate_device_added_for_one_node_replaces_rather_than_duplicates() {
        let mut inventory = InputInventory::new();
        inventory.add(device("/dev/input/event4", "Touchpad v1"));
        inventory.add(device("/dev/input/event4", "Touchpad v2"));
        assert_eq!(
            inventory.len(),
            1,
            "a second DeviceAdded for the same node must replace, not duplicate"
        );
        assert_eq!(
            inventory.get("/dev/input/event4").unwrap().name,
            "Touchpad v2"
        );
    }
}
