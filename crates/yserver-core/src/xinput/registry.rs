//! Source-owned XI device facets.
//!
//! The registry keeps process-local input source identity separate from the
//! reusable eight-bit XI device IDs.  A source record remains present even
//! when there is not enough XI ID capacity to publish all of its facets.

use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
};

use crate::core_loop::DeviceInfo;

use super::{
    DEVICEID_MASTER_KEYBOARD, DEVICEID_MASTER_POINTER, DEVICEID_XTEST_KEYBOARD,
    DEVICEID_XTEST_POINTER, XiDevice, XiQueryError,
};

const FIRST_PHYSICAL_DEVICE_ID: u16 = 6;
const LAST_PHYSICAL_DEVICE_ID: u16 = 127;

pub const NAME_XTEST_POINTER: &str = "Virtual core XTEST pointer";
pub const NAME_XTEST_KEYBOARD: &str = "Virtual core XTEST keyboard";

/// Runtime identity for one backend input source attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputSourceId(pub u64);

/// Capabilities reported for one backend input source.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputCapabilities {
    pub keyboard: bool,
    pub pointer: bool,
    pub touch: bool,
}

/// XI facet kinds supported by the keyboard/pointer registry stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XiFacetKind {
    Keyboard,
    PointerTouch,
}

/// Protocol role of a registered XI device. Slave roles remain the same while
/// detached; attachment is reported separately by [`XiRegistry::attachment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiDeviceRole {
    MasterPointer,
    MasterKeyboard,
    SlavePointer,
    SlaveKeyboard,
}

#[derive(Debug)]
struct SourceRecord {
    info: DeviceInfo,
    facets: HashMap<XiFacetKind, u16>,
}

/// The single owner of live XI devices and input-source metadata.
#[derive(Debug)]
pub struct XiRegistry {
    devices: Vec<XiDevice>,
    sources: HashMap<InputSourceId, SourceRecord>,
}

impl XiRegistry {
    /// Create masters 2/3 and the virtual XTEST slaves 4/5.
    ///
    /// This constructor intentionally has no atom-table dependency; server
    /// initialization seeds the XTEST marker after interning its atom.
    #[must_use]
    pub fn new() -> Self {
        Self {
            devices: vec![
                XiDevice::new(DEVICEID_MASTER_POINTER, super::NAME_MASTER_POINTER),
                XiDevice::new(DEVICEID_MASTER_KEYBOARD, super::NAME_MASTER_KEYBOARD),
                XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER),
                XiDevice::new(DEVICEID_XTEST_KEYBOARD, NAME_XTEST_KEYBOARD),
            ],
            sources: HashMap::new(),
        }
    }

    /// Register all supported keyboard/pointer facets for a source.
    ///
    /// IDs are selected from the lowest free value in 6..=127. Required IDs
    /// are reserved before any device is inserted, so an insufficient range
    /// never publishes only part of a mixed source. Its metadata is still
    /// retained for core delivery and later lookup.
    pub fn register(&mut self, info: &DeviceInfo) -> Vec<u16> {
        if let Some(record) = self.sources.get_mut(&info.source_id) {
            record.info = info.clone();
            let ids = sorted_facet_ids(&record.facets);
            for id in &ids {
                if let Some(device) = self.device_mut(*id) {
                    let was_session_enabled = device.session_enabled;
                    device.session_enabled = info.enabled;
                    device.enabled = device.session_enabled && !device.client_disabled;
                    if !device.enabled && !was_session_enabled {
                        device.attached_master = None;
                    } else if device.enabled && !was_session_enabled {
                        // Enabling restores the role's home master. Do not
                        // use the most recent attachment: grab detachment is
                        // a temporary state, and XI masters never change a
                        // slave's pointer/keyboard role (devices.c:388-394).
                        device.attached_master = Some(match device.facet {
                            Some(XiFacetKind::PointerTouch) => DEVICEID_MASTER_POINTER,
                            Some(XiFacetKind::Keyboard) => DEVICEID_MASTER_KEYBOARD,
                            None => continue,
                        });
                    }
                    device.name.clone_from(&info.name);
                    device.is_touchpad =
                        device.facet == Some(XiFacetKind::PointerTouch) && info.is_touchpad;
                    device.device_node = super::device_node_of(info);
                }
            }
            // A source retained without facets at its first registration
            // stays unpublished during VT continuation. Only a new source
            // attachment may claim newly freed XI IDs.
            return ids;
        }

        let required = facets_for(info.capabilities);
        let free_ids: Vec<u16> = (FIRST_PHYSICAL_DEVICE_ID..=LAST_PHYSICAL_DEVICE_ID)
            .filter(|id| !self.devices.iter().any(|device| device.id == *id))
            .take(required.len())
            .collect();

        let mut facets = HashMap::new();
        if free_ids.len() == required.len() {
            for (facet, id) in required.iter().copied().zip(free_ids.iter().copied()) {
                self.devices.push(XiDevice::physical(id, info, facet));
                facets.insert(facet, id);
            }
            self.devices.sort_by_key(|device| device.id);
        } else if !required.is_empty() {
            log::warn!(
                "xinput: no XI IDs available for source {:?} (needs {}, available {})",
                info.source_id,
                required.len(),
                free_ids.len(),
            );
        }

        let result = sorted_facet_ids(&facets);
        self.sources.insert(
            info.source_id,
            SourceRecord {
                info: info.clone(),
                facets,
            },
        );
        result
    }

    /// Refresh retained source metadata without changing any facet's
    /// session-enabled state. Lifecycle publishers use this before enabling
    /// facets one at a time.
    pub fn refresh_source(&mut self, info: &DeviceInfo) -> Vec<u16> {
        let Some(ids) = self
            .sources
            .get(&info.source_id)
            .map(|record| sorted_facet_ids(&record.facets))
        else {
            return Vec::new();
        };
        for id in &ids {
            if let Some(device) = self.device_mut(*id) {
                device.name.clone_from(&info.name);
                device.is_touchpad =
                    device.facet == Some(XiFacetKind::PointerTouch) && info.is_touchpad;
                device.device_node = super::device_node_of(info);
            }
        }
        let source_enabled = if ids.is_empty() {
            info.enabled
        } else {
            ids.iter().any(|id| {
                self.device(*id)
                    .is_some_and(|device| device.session_enabled)
            })
        };
        if let Some(record) = self.sources.get_mut(&info.source_id) {
            record.info = info.clone();
            record.info.enabled = source_enabled;
        }
        ids
    }

    /// Change one facet's session availability without floating it. A disable
    /// caller emits the hierarchy snapshot first, then floats the facet;
    /// enabling restores the role-derived home master immediately.
    pub fn set_facet_session_enabled(&mut self, device_id: u16, enabled: bool) -> bool {
        let Some(source_id) = self.device(device_id).and_then(|device| device.source_id) else {
            return false;
        };
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        let was_enabled = device.enabled;
        device.session_enabled = enabled;
        device.enabled = device.session_enabled && !device.client_disabled;
        if device.enabled {
            device.attached_master = Some(match device.facet {
                Some(XiFacetKind::PointerTouch) => DEVICEID_MASTER_POINTER,
                Some(XiFacetKind::Keyboard) => DEVICEID_MASTER_KEYBOARD,
                None => return false,
            });
        }
        let is_enabled = device.enabled;

        let any_session_enabled = self
            .sources
            .get(&source_id)
            .into_iter()
            .flat_map(|record| record.facets.values())
            .any(|id| self.device(*id).is_some_and(|facet| facet.session_enabled));
        if let Some(record) = self.sources.get_mut(&source_id) {
            record.info.enabled = any_session_enabled;
        }

        !was_enabled && is_enabled
    }

    /// Record one client's Device Enabled preference independently of the
    /// current input session. Disabling retains the old attachment until
    /// the caller has emitted the XIDeviceDisabled snapshot; enabling only
    /// attaches on a real disabled-to-enabled transition, so a redundant
    /// write cannot undo an XI grab's temporary floating state.
    pub fn set_facet_client_disabled(&mut self, device_id: u16, disabled: bool) -> bool {
        let Some(device) = self.device_mut(device_id) else {
            return false;
        };
        if device.source_id.is_none() || device.client_disabled == disabled {
            return false;
        }

        let was_enabled = device.enabled;
        device.client_disabled = disabled;
        device.enabled = device.session_enabled && !device.client_disabled;
        if !was_enabled && device.enabled {
            device.attached_master = Some(match device.facet {
                Some(XiFacetKind::PointerTouch) => DEVICEID_MASTER_POINTER,
                Some(XiFacetKind::Keyboard) => DEVICEID_MASTER_KEYBOARD,
                None => return false,
            });
        }
        true
    }

    /// Remove one published facet while preserving its source's other facet.
    pub fn remove_facet(&mut self, device_id: u16) -> Option<XiDevice> {
        let removed = self.device(device_id)?.clone();
        let source_id = removed.source_id?;
        let facet = removed.facet?;
        let record = self.sources.get_mut(&source_id)?;
        if record.facets.get(&facet) != Some(&device_id) {
            return None;
        }
        record.facets.remove(&facet);
        let source_empty = record.facets.is_empty();
        if source_empty {
            self.sources.remove(&source_id);
        }
        self.devices.retain(|device| device.id != device_id);
        Some(removed)
    }

    /// Remove a source and all of its currently published facets.
    ///
    /// Returned IDs can be used by later lifecycle stages to publish removal
    /// before those IDs are allocated to a new source.
    pub fn remove(&mut self, source_id: InputSourceId) -> Vec<u16> {
        let Some(record) = self.sources.remove(&source_id) else {
            return Vec::new();
        };
        let mut removed = sorted_facet_ids(&record.facets);
        self.devices
            .retain(|device| device.source_id != Some(source_id));
        removed.sort_unstable();
        removed
    }

    #[must_use]
    pub fn device(&self, device_id: u16) -> Option<&XiDevice> {
        self.devices.iter().find(|device| device.id == device_id)
    }

    pub fn device_mut(&mut self, device_id: u16) -> Option<&mut XiDevice> {
        self.devices
            .iter_mut()
            .find(|device| device.id == device_id)
    }

    /// Select live devices using the XI2 `XIQueryDevice` selectors.
    ///
    /// `XIAllDevices` (0) returns every live device, `XIAllMasterDevices`
    /// (1) returns the paired masters, and any other value selects one exact
    /// live ID. Removed and otherwise unknown IDs report `BadDevice`.
    pub fn query(&self, device_id: u16) -> Result<Vec<&XiDevice>, XiQueryError> {
        match device_id {
            0 => Ok(self.devices.iter().collect()),
            1 => Ok(self
                .devices
                .iter()
                .filter(|device| {
                    matches!(
                        device.id,
                        DEVICEID_MASTER_POINTER | DEVICEID_MASTER_KEYBOARD
                    )
                })
                .collect()),
            id => self
                .device(id)
                .map(|device| vec![device])
                .ok_or(XiQueryError::BadDevice(id)),
        }
    }

    #[must_use]
    pub fn facet(&self, source_id: InputSourceId, kind: XiFacetKind) -> Option<u16> {
        self.sources
            .get(&source_id)
            .and_then(|record| record.facets.get(&kind).copied())
    }

    #[must_use]
    pub fn source(&self, source_id: InputSourceId) -> Option<&DeviceInfo> {
        self.sources.get(&source_id).map(|record| &record.info)
    }

    /// Whether any XI facet still keeps the source's shared libinput handle
    /// active. xf86-input-libinput decrements its shared `enabled_count` per
    /// facet and closes the handle only at zero (`src/xf86libinput.c:416-432`);
    /// its property guard rejects writes only after that handle is null
    /// (`src/xf86libinput.c:4392-4410`).
    #[must_use]
    pub fn source_has_enabled_facet(&self, source_id: InputSourceId) -> bool {
        self.sources
            .get(&source_id)
            .into_iter()
            .flat_map(|record| record.facets.values())
            .any(|device_id| self.device(*device_id).is_some_and(|device| device.enabled))
    }

    pub fn source_mut(&mut self, source_id: InputSourceId) -> Option<&mut DeviceInfo> {
        self.sources
            .get_mut(&source_id)
            .map(|record| &mut record.info)
    }

    /// Every retained physical source, including sources without an
    /// allocated XI facet, ordered by runtime identity.
    #[must_use]
    pub fn source_ids(&self) -> Vec<InputSourceId> {
        let mut source_ids: Vec<_> = self.sources.keys().copied().collect();
        source_ids.sort_unstable_by_key(|source_id| source_id.0);
        source_ids
    }

    #[must_use]
    pub fn role(&self, device_id: u16) -> Option<XiDeviceRole> {
        let device = self.device(device_id)?;
        Some(match device_id {
            DEVICEID_MASTER_POINTER => XiDeviceRole::MasterPointer,
            DEVICEID_MASTER_KEYBOARD => XiDeviceRole::MasterKeyboard,
            DEVICEID_XTEST_POINTER => XiDeviceRole::SlavePointer,
            DEVICEID_XTEST_KEYBOARD => XiDeviceRole::SlaveKeyboard,
            _ => match device.facet? {
                XiFacetKind::PointerTouch => XiDeviceRole::SlavePointer,
                XiFacetKind::Keyboard => XiDeviceRole::SlaveKeyboard,
            },
        })
    }

    /// Current attachment of a slave, or its own master ID for a master.
    #[must_use]
    pub fn attachment(&self, device_id: u16) -> Option<u16> {
        let device = self.device(device_id)?;
        match self.role(device_id)? {
            XiDeviceRole::MasterPointer | XiDeviceRole::MasterKeyboard => Some(device_id),
            XiDeviceRole::SlavePointer | XiDeviceRole::SlaveKeyboard => device.attached_master,
        }
    }

    /// Home master implied by a slave's role. This remains stable while the
    /// slave is temporarily detached for a grab or floated while disabled.
    #[must_use]
    pub fn home_master(&self, device_id: u16) -> Option<u16> {
        match self.role(device_id)? {
            XiDeviceRole::SlavePointer => Some(DEVICEID_MASTER_POINTER),
            XiDeviceRole::SlaveKeyboard => Some(DEVICEID_MASTER_KEYBOARD),
            XiDeviceRole::MasterPointer | XiDeviceRole::MasterKeyboard => None,
        }
    }

    /// Xorg `GetPairedDevice`: the paired master for a master or attached
    /// slave. A floating slave has no current paired master.
    #[must_use]
    pub fn paired_master(&self, device_id: u16) -> Option<u16> {
        use XiDeviceRole::{MasterKeyboard, MasterPointer, SlaveKeyboard, SlavePointer};
        let master = match self.role(device_id)? {
            MasterPointer => DEVICEID_MASTER_POINTER,
            MasterKeyboard => DEVICEID_MASTER_KEYBOARD,
            SlavePointer | SlaveKeyboard => self.attachment(device_id)?,
        };
        match master {
            DEVICEID_MASTER_POINTER => Some(DEVICEID_MASTER_KEYBOARD),
            DEVICEID_MASTER_KEYBOARD => Some(DEVICEID_MASTER_POINTER),
            _ => None,
        }
    }

    #[must_use]
    pub fn devices(&self) -> &[XiDevice] {
        &self.devices
    }
}

impl Default for XiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Temporary compatibility for existing property/query helpers while their
/// callers migrate from a raw slice to explicit registry lookups.
impl Deref for XiRegistry {
    type Target = [XiDevice];

    fn deref(&self) -> &Self::Target {
        &self.devices
    }
}

impl DerefMut for XiRegistry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.devices
    }
}

fn facets_for(capabilities: InputCapabilities) -> Vec<XiFacetKind> {
    let mut facets = Vec::with_capacity(2);
    if capabilities.keyboard {
        facets.push(XiFacetKind::Keyboard);
    }
    // Touch-only sources deliberately receive no XI facet; direct touch is
    // outside this registry's scope.
    if capabilities.pointer {
        facets.push(XiFacetKind::PointerTouch);
    }
    facets
}

fn sorted_facet_ids(facets: &HashMap<XiFacetKind, u16>) -> Vec<u16> {
    let mut ids: Vec<u16> = facets.values().copied().collect();
    ids.sort_unstable();
    ids
}
