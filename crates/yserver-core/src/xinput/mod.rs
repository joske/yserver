//! XI device/source registry and per-device property state.
//!
//! [`XiRegistry`] owns the live device vector, source metadata, and physical
//! facet allocation. Existing protocol consumers retain temporary slice
//! compatibility while their fixed-topology query and event paths migrate.
//! The property map remains on each [`XiDevice`].

use std::collections::BTreeMap;

use yserver_protocol::x11::{AtomId, ClientByteOrder, SequenceNumber};

use crate::core_loop::DeviceInfo;

pub mod hotplug;
pub mod libinput_props;
pub mod query;
pub mod registry;

pub use query::XiQueryError;
pub use registry::{
    InputCapabilities, InputSourceId, NAME_XTEST_KEYBOARD, NAME_XTEST_POINTER, XiDeviceRole,
    XiFacetKind, XiRegistry,
};

// ---------------------------------------------------------------------------
// X11 predefined atom ids for property type annotation
// ---------------------------------------------------------------------------

/// X11 predefined atom `CARDINAL` (id 6).
pub const XA_CARDINAL: AtomId = AtomId(6);

/// X11 predefined atom `INTEGER` (id 19).
pub const XA_INTEGER: AtomId = AtomId(19);

/// X11 predefined atom `STRING` (id 31).
pub const XA_STRING: AtomId = AtomId(31);

// ---------------------------------------------------------------------------
// Device id constants (mirror XIQueryDevice handler)
// ---------------------------------------------------------------------------

pub const DEVICEID_MASTER_POINTER: u16 = 2;
pub const DEVICEID_MASTER_KEYBOARD: u16 = 3;
/// Reserved virtual XTEST pointer; physical pointer facets start at ID 6.
pub const DEVICEID_XTEST_POINTER: u16 = 4;
/// Reserved virtual XTEST keyboard; physical keyboard facets start at ID 6.
pub const DEVICEID_XTEST_KEYBOARD: u16 = 5;

pub const NAME_MASTER_POINTER: &str = "Virtual core pointer";
pub const NAME_MASTER_KEYBOARD: &str = "Virtual core keyboard";
/// XI2 `XI_DeviceChanged` event-mask bit (`1 << XI_DeviceChanged`,
/// XI2.h:232). Single source of truth shared by `process_request`'s
/// XISelectEvents bootstrap and the touchpad-add/remove fanout so both
/// match the same selection bit.
pub const XI2_DEVICE_CHANGED_MASK: u32 = 1 << 1;

/// XI2 `XI_HierarchyChanged` event number (XI2.h:99).
pub const XI2_HIERARCHY_CHANGED_EVENT_TYPE: u32 = 11;

/// XI2 `XI_HierarchyChanged` selection bit (`1 << 11`, XI2.h:101).
pub const XI2_HIERARCHY_CHANGED_MASK: u32 = 1 << XI2_HIERARCHY_CHANGED_EVENT_TYPE;

/// XI2 `XI_PropertyEvent` event-mask bit (`1 << XI_PropertyEvent`,
/// XI2.h:243). Single source of truth shared by `emit_property_change`
/// and any test that wants to subscribe a client to the
/// device-property change/delete fan-out.
pub const XI2_PROPERTY_EVENT_MASK: u32 = 1 << 12;

/// XI 1.x `DevicePresenceNotify` event offset within the XInput event block.
pub const XI_DEVICE_PRESENCE_NOTIFY_OFFSET: u8 = 15;

/// Special XI1 `XEventClass` used to select device-presence notifications.
/// Xorg reserves device id 256 for classes which are not bound to a device;
/// `_devicePresence` is class offset zero.
pub const XI1_DEVICE_PRESENCE_CLASS: u32 = 256 << 8;

/// XI 1.x `DevicePropertyNotify` event code (XIproto.h:139, the 17th
/// XInput event type). The wire type byte for a delivered event is
/// `first_event + XI_DEVICE_PROPERTY_NOTIFY_OFFSET` (= 82 with
/// yserver's `XI_FIRST_EVENT = 66`); the same value appears as the low
/// byte of every `XEventClass` a client supplies via
/// `SelectExtensionEvent` (XInput minor 6) when subscribing to
/// device-property notifications.
pub const XI_DEVICE_PROPERTY_NOTIFY_OFFSET: u8 = 16;

/// XI 1.x event-code offsets within the XInput event block
/// (`XI_FIRST_EVENT + offset`), per Xorg `Xi/extinit.c` event order:
/// DeviceValuator(0), DeviceKeyPress(1), DeviceKeyRelease(2),
/// DeviceButtonPress(3), DeviceButtonRelease(4), DeviceMotionNotify(5),
/// DeviceFocusIn(6), DeviceFocusOut(7), ProximityIn(8), ProximityOut(9),
/// DeviceStateNotify(10), DeviceMappingNotify(11), ChangeDeviceNotify(12).
pub const XI_DEVICE_VALUATOR_OFFSET: u8 = 0;
pub const XI_DEVICE_KEY_PRESS_OFFSET: u8 = 1;
pub const XI_DEVICE_KEY_RELEASE_OFFSET: u8 = 2;
pub const XI_DEVICE_BUTTON_PRESS_OFFSET: u8 = 3;
pub const XI_DEVICE_BUTTON_RELEASE_OFFSET: u8 = 4;
pub const XI_DEVICE_MOTION_NOTIFY_OFFSET: u8 = 5;
pub const XI_DEVICE_FOCUS_IN_OFFSET: u8 = 6;
pub const XI_DEVICE_FOCUS_OUT_OFFSET: u8 = 7;
pub const XI_DEVICE_STATE_NOTIFY_OFFSET: u8 = 10;
pub const XI_DEVICE_MAPPING_NOTIFY_OFFSET: u8 = 11;
pub const XI_CHANGE_DEVICE_NOTIFY_OFFSET: u8 = 12;
/// DeviceKeyStateNotify(13) / DeviceButtonStateNotify(14) — the
/// "reserved space of 3" continuation codes after ChangeDeviceNotify
/// (XIproto.h:114-118). Only ever sent as MORE_EVENTS continuations of
/// a DeviceStateNotify, never selected directly.
pub const XI_DEVICE_KEY_STATE_NOTIFY_OFFSET: u8 = 13;
pub const XI_DEVICE_BUTTON_STATE_NOTIFY_OFFSET: u8 = 14;

/// `deviceid` high bit marking "another event of this logical event
/// follows" in DeviceStateNotify / DeviceValuator chains
/// (XIproto.h:67). libXi buffers a chain until it sees a deviceid
/// without this bit, then enqueues one reassembled client event.
pub const XI1_MORE_EVENTS: u8 = 0x80;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single XI2 device property value.
///
/// `format` is the X11 wire format: 8, 16, or 32 bits per element.
/// `data` is raw little-endian bytes (`format / 8` bytes per element).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XiProperty {
    pub type_atom: AtomId,
    pub format: u8,
    pub data: Vec<u8>,
    /// Whether clients may replace or append to this property.
    pub read_only: bool,
    /// Whether direct XI1/XI2 DeleteProperty requests may unlink it.
    pub deletable: bool,
}

/// One entry in the XI2 device registry.
#[derive(Debug, Clone)]
pub struct XiDevice {
    pub id: u16,
    pub name: String,
    /// Whether the device is currently enabled and can receive input.
    pub enabled: bool,
    /// Physical source owning this facet; virtual and master devices have none.
    pub source_id: Option<InputSourceId>,
    /// Physical capability represented by this facet.
    pub facet: Option<XiFacetKind>,
    /// Master this slave is attached to, when it is a slave device.
    pub attached_master: Option<u16>,
    /// True when the pointer facet is a touchpad. Set from source metadata
    /// when registered; the legacy slice helper also maintains it. Used to
    /// select the XI 1.x device-type atom
    /// (`TOUCHPAD` vs `MOUSE`) in the `XListInputDevices` reply.
    pub is_touchpad: bool,
    /// Evdev device node (`/dev/input/eventN`) for the physical source bound
    /// to this facet, or `None` for a master or virtual device. Descriptive
    /// metadata only (it backs the read-only `Device Node` property): writes
    /// and events are routed by `source_id`, never by node, since a node can
    /// be reused by a later attachment.
    pub device_node: Option<String>,
    /// Properties keyed by their name-atom (`AtomId`).  `BTreeMap` gives
    /// stable, sorted iteration order for XIListProperties (Task 3);
    /// `AtomId` derives `Ord` (an arbitrary-but-stable ordering over the
    /// inner `u32`), so it is used directly as the key.
    pub properties: BTreeMap<AtomId, XiProperty>,
    /// Logical buttons currently held by this pointer facet. Button `n` is
    /// represented by bit `n - 1`; each slave/XTEST device owns its own set.
    pub buttons_down: u16,
    /// Cumulative vertical and horizontal smooth-scroll valuators for this
    /// pointer source, independent of other attached mice.
    pub scroll_axis_values: [i32; 2],
}

impl XiDevice {
    fn new(id: u16, name: &str) -> Self {
        Self {
            id,
            name: name.to_owned(),
            enabled: true,
            source_id: None,
            facet: None,
            attached_master: match id {
                DEVICEID_XTEST_POINTER => Some(DEVICEID_MASTER_POINTER),
                DEVICEID_XTEST_KEYBOARD => Some(DEVICEID_MASTER_KEYBOARD),
                _ => None,
            },
            is_touchpad: false,
            device_node: None,
            properties: BTreeMap::new(),
            buttons_down: 0,
            scroll_axis_values: [0; 2],
        }
    }

    pub(crate) fn physical(id: u16, info: &DeviceInfo, facet: XiFacetKind) -> Self {
        let attached_master = match facet {
            XiFacetKind::Keyboard => DEVICEID_MASTER_KEYBOARD,
            XiFacetKind::PointerTouch => DEVICEID_MASTER_POINTER,
        };
        Self {
            id,
            name: info.name.clone(),
            enabled: info.enabled,
            source_id: Some(info.source_id),
            facet: Some(facet),
            attached_master: Some(attached_master),
            is_touchpad: facet == XiFacetKind::PointerTouch && info.is_touchpad,
            device_node: Some(info.device_node.clone()),
            properties: BTreeMap::new(),
            buttons_down: 0,
            scroll_axis_values: [0; 2],
        }
    }
}

// ---------------------------------------------------------------------------
// Initial registry (mirrors the static XIQueryDevice device list)
// ---------------------------------------------------------------------------

/// Build the historical four-entry device slice for legacy property helpers.
/// Server state uses [`XiRegistry::new`] for its live device ownership.
pub fn initial_xi_devices() -> Vec<XiDevice> {
    vec![
        XiDevice::new(DEVICEID_MASTER_POINTER, NAME_MASTER_POINTER),
        XiDevice::new(DEVICEID_MASTER_KEYBOARD, NAME_MASTER_KEYBOARD),
        XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER),
        XiDevice::new(DEVICEID_XTEST_KEYBOARD, NAME_XTEST_KEYBOARD),
    ]
}

// ---------------------------------------------------------------------------
// Property-name atom interning helpers
// ---------------------------------------------------------------------------

/// Names of libinput properties that we populate from `DeviceInfo`.
///
/// Kept as a top-level constant so that protocol tests and the XI 1.x
/// XListInputDevices encoder share a single canonical spelling with the
/// descriptor table.
pub const PROP_TAPPING_ENABLED: &str = "libinput Tapping Enabled";
/// XTEST's predefined virtual-device marker property.
pub const PROP_XTEST_DEVICE: &str = "XTEST Device";
const PROP_DEVICE_NODE: &str = "Device Node";
const PROP_DEVICE_PRODUCT_ID: &str = "Device Product ID";

/// Encode two u32 values as 8 little-endian bytes (for Device Product ID).
fn encode_product_id(vendor: u32, product: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&vendor.to_le_bytes());
    buf.extend_from_slice(&product.to_le_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Seed / clear logic
// ---------------------------------------------------------------------------

/// True if the descriptor's `binding` is available in `config`.
///
/// Read-only group-companions (`…Available`) are gated by their group's
/// `available_mask != 0` (or `.available` for the OneHot2 groups). Read-
/// only `…Default` companions are gated by the binding of their
/// writable sibling (looked up by stripping the suffix); a `…Default`
/// row with no sibling is treated as always-on (defensive — every
/// Default in `DESCRIPTORS` has one).
pub(crate) fn descriptor_available(
    desc: &libinput_props::PropDescriptor,
    config: &crate::core_loop::message::LibinputConfigSnapshot,
) -> bool {
    use libinput_props::Access;

    if let Some(binding) = desc.binding {
        return binding_available(binding, config);
    }
    debug_assert_eq!(desc.access, Access::ReadOnly);
    match desc.name {
        "libinput Scroll Methods Available" => config.scroll_method.available_mask != 0,
        "libinput Click Methods Available" => config.click_method.available,
        "libinput Send Events Modes Available" => config.send_events.available_mask != 0,
        "libinput Accel Profiles Available" => config.accel_profile_available_mask != 0,
        _ => default_companion_available(desc.name, config),
    }
}

/// Whether the writable sibling of a `…Default` row is available.
fn default_companion_available(
    name: &str,
    config: &crate::core_loop::message::LibinputConfigSnapshot,
) -> bool {
    let Some(sibling) = writable_sibling_for_default(name) else {
        return true;
    };
    sibling.binding.is_none_or(|b| binding_available(b, config))
}

/// Look up the writable sibling for a `…Default` ReadOnly row.
///
/// Most defaults follow the `<writable> Default` convention, so simply
/// stripping the suffix and looking up the writable works. The Tapping
/// Button Mapping pair is the lone exception: the writable is
/// `…Mapping Enabled` and the companion is `…Mapping Default` (no
/// `Enabled` in the default's name) — handled by name here.
fn writable_sibling_for_default(name: &str) -> Option<&'static libinput_props::PropDescriptor> {
    if name == "libinput Tapping Button Mapping Default" {
        return libinput_props::descriptor_by_name("libinput Tapping Button Mapping Enabled");
    }
    let writable_name = name.strip_suffix(" Default")?;
    libinput_props::descriptor_by_name(writable_name)
}

/// Per-`Binding` availability predicate.
pub(crate) fn binding_available(
    binding: libinput_props::Binding,
    config: &crate::core_loop::message::LibinputConfigSnapshot,
) -> bool {
    use libinput_props::Binding;
    match binding {
        Binding::Tap => config.tap.available,
        Binding::TapDrag => config.tap_drag.available,
        Binding::TapDragLock => config.tap_drag_lock.available,
        Binding::TapButtonMap => config.tap_button_map.available,
        Binding::NaturalScroll => config.natural_scroll.available,
        Binding::Dwt => config.dwt.available,
        Binding::LeftHanded => config.left_handed.available,
        Binding::MiddleEmulation => config.middle_emulation.available,
        Binding::ScrollMethod => config.scroll_method.available_mask != 0,
        Binding::ClickMethod => config.click_method.available,
        Binding::SendEvents => config.send_events.available_mask != 0,
        Binding::AccelSpeed => config.accel.available,
        Binding::AccelProfile => config.accel_profile_available_mask != 0,
        Binding::ScrollButton => config.scroll_button.available,
        Binding::ScrollButtonLock => config.scroll_button_lock.available,
    }
}

/// Compute the on-wire data bytes for a descriptor's `current` value from
/// the snapshot. Read-only `…Default` rows pull the matching `default`
/// instead via [`descriptor_default_data`]; `…Available` rows pull a
/// mask via [`descriptor_available_data`].
fn descriptor_current_data(
    desc: &libinput_props::PropDescriptor,
    config: &crate::core_loop::message::LibinputConfigSnapshot,
) -> Vec<u8> {
    use libinput_props::{
        Binding, encode_bitflags, encode_bool, encode_card32, encode_float, encode_onehot,
    };
    match desc.binding {
        Some(Binding::Tap) => encode_bool(config.tap.current),
        Some(Binding::TapDrag) => encode_bool(config.tap_drag.current),
        Some(Binding::TapDragLock) => encode_bool(config.tap_drag_lock.current),
        Some(Binding::TapButtonMap) => encode_onehot(config.tap_button_map.current, 2),
        Some(Binding::NaturalScroll) => encode_bool(config.natural_scroll.current),
        Some(Binding::Dwt) => encode_bool(config.dwt.current),
        Some(Binding::LeftHanded) => encode_bool(config.left_handed.current),
        Some(Binding::MiddleEmulation) => encode_bool(config.middle_emulation.current),
        Some(Binding::ScrollMethod) => encode_onehot(config.scroll_method.current, 3),
        Some(Binding::ClickMethod) => encode_onehot(config.click_method.current, 2),
        Some(Binding::SendEvents) => encode_bitflags(config.send_events.current_mask, 2),
        Some(Binding::AccelSpeed) => encode_float(config.accel.current),
        // Snapshot's `accel_profile` is OneHot2 (custom slot excluded);
        // wire layout is 3-wide, so widen at the encode site.
        Some(Binding::AccelProfile) => encode_onehot(config.accel_profile.current, 3),
        Some(Binding::ScrollButton) => encode_card32(config.scroll_button.current),
        Some(Binding::ScrollButtonLock) => encode_bool(config.scroll_button_lock.current),
        None => Vec::new(),
    }
}

/// Compute the on-wire data bytes for a `…Default` row's value.
fn descriptor_default_data(
    desc: &libinput_props::PropDescriptor,
    config: &crate::core_loop::message::LibinputConfigSnapshot,
) -> Option<Vec<u8>> {
    use libinput_props::{
        encode_bitflags, encode_bool, encode_card32, encode_float, encode_onehot,
    };
    let sibling = writable_sibling_for_default(desc.name)?;
    let binding = sibling.binding?;
    use libinput_props::Binding;
    Some(match binding {
        Binding::Tap => encode_bool(config.tap.default),
        Binding::TapDrag => encode_bool(config.tap_drag.default),
        Binding::TapDragLock => encode_bool(config.tap_drag_lock.default),
        Binding::TapButtonMap => encode_onehot(config.tap_button_map.default, 2),
        Binding::NaturalScroll => encode_bool(config.natural_scroll.default),
        Binding::Dwt => encode_bool(config.dwt.default),
        Binding::LeftHanded => encode_bool(config.left_handed.default),
        Binding::MiddleEmulation => encode_bool(config.middle_emulation.default),
        Binding::ScrollMethod => encode_onehot(config.scroll_method.default, 3),
        Binding::ClickMethod => encode_onehot(config.click_method.default, 2),
        Binding::SendEvents => encode_bitflags(config.send_events.default_mask, 2),
        Binding::AccelSpeed => encode_float(config.accel.default),
        Binding::AccelProfile => encode_onehot(config.accel_profile.default, 3),
        Binding::ScrollButton => encode_card32(config.scroll_button.default),
        Binding::ScrollButtonLock => encode_bool(config.scroll_button_lock.default),
    })
}

/// Compute the on-wire data bytes for an `…Available` row (BitFlags
/// over the group's `available_mask`).
fn descriptor_available_data(
    name: &str,
    config: &crate::core_loop::message::LibinputConfigSnapshot,
) -> Option<Vec<u8>> {
    use libinput_props::encode_bitflags;
    Some(match name {
        "libinput Scroll Methods Available" => {
            encode_bitflags(config.scroll_method.available_mask, 3)
        }
        // Click methods is `OneHot2` (snapshot stores `.available` only);
        // when supported, libinput offers both methods (button-areas +
        // clickfinger) — encode `0b11` so the wire surface matches the
        // Xorg driver.
        "libinput Click Methods Available" => {
            let mask = if config.click_method.available {
                0b11
            } else {
                0
            };
            encode_bitflags(mask, 2)
        }
        "libinput Send Events Modes Available" => {
            encode_bitflags(config.send_events.available_mask, 2)
        }
        // Keep the Xorg 3-slot wire value even though the current build
        // only exposes adaptive/flat setters.
        "libinput Accel Profiles Available" => {
            encode_bitflags(config.accel_profile_available_mask, 3)
        }
        _ => return None,
    })
}

/// Map a descriptor's [`libinput_props::XiValType`] to its X11 type
/// atom: `INTEGER` for Bool, `CARDINAL` for Card32, the runtime-
/// interned FLOAT atom for Float.
///
/// Also used by `core_loop::process_request::dispatch_change_property`
/// (T3 B1) to check an incoming write's `type_atom` against the
/// descriptor's declared type, alongside the `format` check — pass
/// `ServerState::float_atom`.
#[must_use]
pub fn type_atom_for(val: libinput_props::XiValType, float_atom: AtomId) -> AtomId {
    match val {
        libinput_props::XiValType::Bool => XA_INTEGER,
        libinput_props::XiValType::Card32 => XA_CARDINAL,
        libinput_props::XiValType::Float => float_atom,
    }
}

/// Seed the source metadata and supported pointer properties on one physical facet.
///
/// Every physical facet receives its own Device Node and Device Product ID.
/// Only pointer facets receive the available libinput descriptors from
/// their source's own configuration snapshot. `float_atom` is the server-wide
/// FLOAT atom (`ServerState::float_atom`).
pub fn seed_pointer_properties(
    device: &mut XiDevice,
    atoms: &mut crate::server::AtomTable,
    float_atom: AtomId,
    info: &DeviceInfo,
) {
    if device.source_id != Some(info.source_id) {
        return;
    }

    device.name = info.name.clone();
    device.is_touchpad = device.facet == Some(XiFacetKind::PointerTouch) && info.is_touchpad;
    device.device_node = Some(info.device_node.clone());

    // Fixed identity properties: Device Node + Device Product ID.
    let node_atom = atoms.intern(PROP_DEVICE_NODE, false);
    set_seeded_property(
        device,
        node_atom,
        XA_STRING,
        8,
        info.device_node.as_bytes().to_vec(),
        true,
        false,
    );
    let pid_atom = atoms.intern(PROP_DEVICE_PRODUCT_ID, false);
    set_seeded_property(
        device,
        pid_atom,
        XA_INTEGER,
        32,
        encode_product_id(info.vendor_id, info.product_id),
        true,
        false,
    );

    if device.facet != Some(XiFacetKind::PointerTouch) || !info.capabilities.pointer {
        return;
    }

    // Table-driven libinput properties.
    for desc in libinput_props::DESCRIPTORS {
        if !descriptor_available(desc, &info.config) {
            continue;
        }
        // Explicit match on Access makes the
        // `binding.is_some() ⇔ ReadWrite` invariant compiler-checked;
        // and `expect` over `unwrap_or_default` turns any future row
        // that's added to the table but forgotten in the data helpers
        // into a loud panic rather than a silent empty-bytes seed.
        let data = match desc.access {
            libinput_props::Access::ReadWrite => descriptor_current_data(desc, &info.config),
            libinput_props::Access::ReadOnly => {
                if desc.name.ends_with(" Default") {
                    descriptor_default_data(desc, &info.config).unwrap_or_else(|| {
                        panic!(
                            "descriptor `{}` missing from descriptor_default_data — \
                             add a writable sibling lookup or extend the binding match",
                            desc.name
                        )
                    })
                } else {
                    descriptor_available_data(desc.name, &info.config).unwrap_or_else(|| {
                        panic!(
                            "descriptor `{}` missing from descriptor_available_data — \
                             extend the helper for any new …Available row",
                            desc.name
                        )
                    })
                }
            }
        };
        let atom = atoms.intern(desc.name, false);
        set_seeded_property(
            device,
            atom,
            type_atom_for(desc.val, float_atom),
            desc.format,
            data,
            desc.access == libinput_props::Access::ReadOnly,
            false,
        );
    }
}

fn set_seeded_property(
    device: &mut XiDevice,
    atom: AtomId,
    type_atom: AtomId,
    format: u8,
    data: Vec<u8>,
    read_only: bool,
    deletable: bool,
) {
    if let Some(property) = device.properties.get_mut(&atom) {
        property.type_atom = type_atom;
        property.format = format;
        property.data = data;
        // Continuation refresh changes values only; ownership and client
        // deletion state belong to the existing property record.
    } else {
        device.properties.insert(
            atom,
            XiProperty {
                type_atom,
                format,
                data,
                read_only,
                deletable,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// XI2 device-property protocol (Tier 2 Task 3)
// ---------------------------------------------------------------------------
//
// Wire layouts sourced from the system XInput2 headers and the X.Org
// reference implementation:
//
//   * /usr/include/X11/extensions/XI2proto.h
//       xXIListPropertiesReply  (lines 751-764, sz = 32)
//       xXIGetPropertyReq       (lines 798-814, sz = 24)
//       xXIGetPropertyReply     (lines 816-830, sz = 32)
//       xXIChangePropertyReq    (lines 769-780, sz = 20)
//       xXIDeletePropertyReq    (lines 785-793, sz = 12)
//   * /usr/include/X11/extensions/XI2.h
//       XIPropModeReplace/Prepend/Append (lines 41-43)
//       XIAnyPropertyType (line 46)
//   * /home/jos/Projects/xserver/Xi/xiproperty.c
//       get_property()            (lines 238-319)  — GetProperty windowing/
//                                                    mismatch/not-found semantics
//       check_change_property()   (lines 321-...)  — format validation
//       XIChangeDeviceProperty()  (lines 684-805)  — Replace/Prepend/Append
//       ProcXIListProperties()    (lines 1089-1123)
//       ProcXIChangeProperty()    (lines 1125-1156)
//       ProcXIDeleteProperty()    (lines 1159-1179)
//       ProcXIGetProperty()       (lines 1181-1253)
//
// The reply byte 0 (`repType`) is `X_Reply` (1) in the XI2proto structs;
// `fixed_reply` in the protocol crate already emits 1 there, matching
// every other XI2 reply arm in process_request.rs.

/// XI2 ChangeProperty mode: overwrite the existing value.
pub const XI_PROP_MODE_REPLACE: u8 = 0;
/// XI2 ChangeProperty mode: prepend to the existing value.
pub const XI_PROP_MODE_PREPEND: u8 = 1;
/// XI2 ChangeProperty mode: append to the existing value.
pub const XI_PROP_MODE_APPEND: u8 = 2;

/// `XIAnyPropertyType` (XI2.h:46) — the wildcard type-match atom.
pub const XI_ANY_PROPERTY_TYPE: AtomId = AtomId(0);

/// X11 protocol error codes used by the property handlers.
///
/// `BadValue`, `BadMatch`, `BadAtom`, and `BadLength` are core (global)
/// codes; `BadDevice` is the XInput-extension-relative code
/// (`XI_BadDevice = 0`, so the wire code is `first_error + 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiPropError {
    /// Device id not found (`XI_BadDevice`, extension-relative).
    BadDevice,
    /// Bad format / value (core `BadValue` = 2).
    BadValue,
    /// Append/Prepend type-or-format mismatch (core `BadMatch` = 8).
    BadMatch,
}

/// The `what` payload byte for `XI_PropertyEvent` / XI1
/// `DevicePropertyNotify`.
///
/// Numeric values are the on-the-wire encoding (`XI2.h` lines 36–38):
/// `XIPropertyDeleted = 0`, `XIPropertyCreated = 1`,
/// `XIPropertyModified = 2`. Shared by the XI2 `XI_PropertyEvent` and
/// XI1 `DevicePropertyNotify` emit paths so neither can disagree on
/// the byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropWhat {
    /// Property was removed (`XIDeleteProperty` succeeded, or
    /// `XIGetProperty(delete=1)` consumed the full value).
    Deleted = 0,
    /// Property did not exist before this `XIChangeProperty` write.
    Created = 1,
    /// Property already existed; this `XIChangeProperty` modified it.
    Modified = 2,
}

/// Look up a device by id.
#[must_use]
pub fn find_device(devices: &[XiDevice], deviceid: u16) -> Option<&XiDevice> {
    devices.iter().find(|d| d.id == deviceid)
}

/// Return the registry name for `deviceid`, falling back to the static
/// per-id default if the device is absent.
///
/// This is the single source of truth that BOTH the XI2 `XIQueryDevice`
/// (opcode 48) and the XI1 `XListInputDevices` encoders read, so the two
/// enumerations can never disagree on a device name (real Chromium /
/// Electron clients cross-check the two and fatal-`CHECK` on a mismatch —
/// the documented `ListInputDevices` crash class).
#[must_use]
pub fn device_name(devices: &[XiDevice], deviceid: u16) -> &str {
    if let Some(dev) = find_device(devices, deviceid) {
        return dev.name.as_str();
    }
    match deviceid {
        DEVICEID_MASTER_POINTER => NAME_MASTER_POINTER,
        DEVICEID_MASTER_KEYBOARD => NAME_MASTER_KEYBOARD,
        DEVICEID_XTEST_KEYBOARD => NAME_XTEST_KEYBOARD,
        DEVICEID_XTEST_POINTER => NAME_XTEST_POINTER,
        _ => {
            // An id we don't know about should never reach here: the
            // four bootstrap devices are 2/3/4/5. Be loud in debug so a
            // stray id surfaces in tests/dev, but stay harmless in
            // release by falling back to the XTEST-pointer name.
            debug_assert!(false, "device_name: unknown device id {deviceid}");
            NAME_XTEST_POINTER
        }
    }
}

/// Look up a device by id (mutable).
pub fn find_device_mut(devices: &mut [XiDevice], deviceid: u16) -> Option<&mut XiDevice> {
    devices.iter_mut().find(|d| d.id == deviceid)
}

/// Return `true` if the device with `deviceid` is currently a touchpad.
///
/// Used by the XI 1.x `XListInputDevices` encoder to pick the correct
/// device-type atom (`TOUCHPAD` vs `MOUSE`) for the slave-pointer entry.
#[must_use]
pub fn device_is_touchpad(devices: &[XiDevice], deviceid: u16) -> bool {
    find_device(devices, deviceid).is_some_and(|d| d.is_touchpad)
}

// ---------------------------------------------------------------------------
// XI device-type atom name constants (XI.h: XI_MOUSE, XI_KEYBOARD,
// XI_TOUCHPAD).  These strings are interned into the atom table at server
// start so that clients calling InternAtom(only_if_exists=true) at session
// init (e.g. MATE's settings daemon) find them before listing devices.
// ---------------------------------------------------------------------------

/// XI 1.x device-type atom name for pointer devices (XI.h: `XI_MOUSE`).
pub const XI_ATOM_MOUSE: &str = "MOUSE";
/// XI 1.x device-type atom name for keyboard devices (XI.h: `XI_KEYBOARD`).
pub const XI_ATOM_KEYBOARD: &str = "KEYBOARD";
/// XI 1.x device-type atom name for touchpad devices (XI.h: `XI_TOUCHPAD`).
pub const XI_ATOM_TOUCHPAD: &str = "TOUCHPAD";

/// Pad `out` with zero bytes up to the next 4-byte boundary.
fn pad_to_4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Encode an `XIListProperties` reply.
///
/// Layout (`xXIListPropertiesReply`, XI2proto.h:751-764, 32-byte header):
/// ```text
///   0   repType        = 1 (X_Reply)
///   1   RepType        = 0 (pad in our encoder; the major opcode is not echoed)
///   2   sequenceNumber
///   4   length         = num_properties (one CARD32 atom each → length in 4-byte units == count)
///   8   num_properties (CARD16)
///  10   pad0 (CARD16)
///  12   pad1..pad5     (20 bytes)
///  32   num_properties * Atom (CARD32) follows
/// ```
#[must_use]
pub fn encode_list_properties_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    device: &XiDevice,
) -> Vec<u8> {
    let atoms: Vec<AtomId> = device.properties.keys().copied().collect();
    #[allow(clippy::cast_possible_truncation)]
    let num = atoms.len() as u16;

    let mut reply = fixed_header(byte_order, sequence, u32::from(num));
    write_u16(byte_order, &mut reply, num); // bytes 8-9: num_properties
    reply.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad0..pad5
    debug_assert_eq!(reply.len(), 32);
    for atom in atoms {
        write_u32(byte_order, &mut reply, atom.0);
    }
    reply
}

/// Result of the `get_property` value-window computation.
struct GetPropertyResult {
    type_atom: AtomId,
    format: u8,
    bytes_after: u32,
    num_items: u32,
    /// The actual value bytes to emit after the 32-byte reply header.
    data: Vec<u8>,
}

/// Compute the `XIGetProperty` reply fields, mirroring xserver's
/// `get_property()` (xiproperty.c:238-319).
///
/// `offset` and `len` are in 4-byte units exactly as they arrive on the
/// wire (`xXIGetPropertyReq.offset` / `.len`).  Returns `None` only when
/// the offset is invalid (xserver returns `BadValue`); the caller maps
/// `None` to `BadValue`.
fn compute_get_property(
    prop: Option<&XiProperty>,
    requested_type: AtomId,
    offset: u32,
    len: u32,
) -> Option<GetPropertyResult> {
    let Some(prop) = prop else {
        // Property absent → type=None, everything zero (xiproperty.c:266-273).
        return Some(GetPropertyResult {
            type_atom: AtomId(0),
            format: 0,
            bytes_after: 0,
            num_items: 0,
            data: Vec::new(),
        });
    };

    // Type mismatch (and not AnyPropertyType): return metadata, no data.
    // xiproperty.c:284-291 — bytes_after = prop_value->size (item count).
    if requested_type != XI_ANY_PROPERTY_TYPE && requested_type != prop.type_atom {
        let item_size = usize::from(prop.format / 8).max(1);
        #[allow(clippy::cast_possible_truncation)]
        let size_items = (prop.data.len() / item_size) as u32;
        return Some(GetPropertyResult {
            type_atom: prop.type_atom,
            format: prop.format,
            bytes_after: size_items,
            num_items: 0,
            data: Vec::new(),
        });
    }

    // Value window. n = total bytes; ind = offset*4 (xiproperty.c:294-316).
    let n = prop.data.len();
    let ind = (offset as usize).saturating_mul(4);
    if n < ind {
        // Offset past end → BadValue (xiproperty.c:300-303).
        return None;
    }
    let avail = n - ind;
    let window = std::cmp::min(avail, (len as usize).saturating_mul(4));
    #[allow(clippy::cast_possible_truncation)]
    let bytes_after = (n - (ind + window)) as u32;
    let num_items = if prop.format == 0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let ni = (window / usize::from(prop.format / 8)) as u32;
        ni
    };
    Some(GetPropertyResult {
        type_atom: prop.type_atom,
        format: prop.format,
        bytes_after,
        num_items,
        data: prop.data[ind..ind + window].to_vec(),
    })
}

/// Encode an `XIGetProperty` reply and (if requested + fully consumed)
/// delete the property from `device`.
///
/// Layout (`xXIGetPropertyReply`, XI2proto.h:816-830, 32-byte header):
/// ```text
///   0   repType = 1     8   type (Atom/CARD32)
///   1   RepType = 0    12   bytes_after (CARD32)
///   2   sequenceNumber 16   num_items (CARD32)
///   4   length          20  format (CARD8)
///                       21  pad0 (CARD8)
///                       22  pad1 (CARD16)
///                       24  pad2 (CARD32)
///                       28  pad3 (CARD32)
///   32  value bytes (length bytes, then pad to 4)
/// ```
/// `length` field (bytes 4-7) is the value length in 4-byte units.
///
/// On `delete && matched && bytes_after == 0` the property is removed
/// from the map (xiproperty.c:1239-1250). Returns `Err(BadValue)` on a
/// bad offset (xiproperty.c:300-303).
///
/// # Errors
/// Returns [`XiPropError::BadValue`] when `offset` lies past the end of
/// the stored value.
pub fn encode_get_property_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    device: &mut XiDevice,
    property: AtomId,
    requested_type: AtomId,
    offset: u32,
    len: u32,
    delete: bool,
) -> Result<Vec<u8>, XiPropError> {
    let result = compute_get_property(
        device.properties.get(&property),
        requested_type,
        offset,
        len,
    )
    .ok_or(XiPropError::BadValue)?;

    // length field = bytes_to_int32(value_bytes) = ceil(bytes/4)
    // (xiproperty.c:1211, `bytes_to_int32(length)`).
    let length_field = u32::try_from(result.data.len().div_ceil(4)).unwrap_or(0);

    let mut reply = fixed_header(byte_order, sequence, length_field);
    write_u32(byte_order, &mut reply, result.type_atom.0); // bytes 8-11
    write_u32(byte_order, &mut reply, result.bytes_after); // bytes 12-15
    write_u32(byte_order, &mut reply, result.num_items); // bytes 16-19
    reply.push(result.format); // byte 20
    reply.extend_from_slice(&[0u8; 11]); // bytes 21-31: pad0..pad3
    debug_assert_eq!(reply.len(), 32);

    match (byte_order, result.format) {
        (ClientByteOrder::BigEndian, 16) => {
            for value in result.data.chunks_exact(2) {
                reply.extend_from_slice(&[value[1], value[0]]);
            }
            reply.extend_from_slice(result.data.chunks_exact(2).remainder());
        }
        (ClientByteOrder::BigEndian, 32) => {
            for value in result.data.chunks_exact(4) {
                reply.extend_from_slice(&[value[3], value[2], value[1], value[0]]);
            }
            reply.extend_from_slice(result.data.chunks_exact(4).remainder());
        }
        _ => reply.extend_from_slice(&result.data),
    }
    pad_to_4(&mut reply);

    // Delete-after-read gate, exactly as xserver's ProcXIGetProperty
    // (xiproperty.c:1239-1250): remove when `delete && bytes_after == 0`.
    // `BTreeMap::remove` is a no-op for the absent / not-fully-matched
    // paths (where the property either isn't present or bytes_after > 0).
    if delete && result.bytes_after == 0 {
        // The dispatch arm observes the removal by snapshotting
        // `device.properties.contains_key(property)` either side of this
        // call and emits `XI_PropertyEvent(Deleted)` there (T5). Kept
        // here as a pure mutation — the protocol-level event helper is
        // not reachable from `xinput/mod.rs` without dragging
        // `ServerState` in.
        device.properties.remove(&property);
    }

    Ok(reply)
}

/// Apply an `XIChangeProperty` request to `device`, mirroring
/// `XIChangeDeviceProperty()` (xiproperty.c:684-805).
///
/// `format` must be 8, 16, or 32 (`BadValue` otherwise, per
/// `check_change_property`, xiproperty.c:330-333). For Prepend/Append the
/// existing property's `format` and `type` must match (`BadMatch`,
/// xiproperty.c:714-717).
///
/// `data` is the raw value bytes (`num_items * format/8`).
///
/// Returns `PropWhat::Created` when the property was absent before this
/// write, and `PropWhat::Modified` when it already existed. This is the
/// value the dispatch layer feeds into `XI_PropertyEvent.what` so the
/// observer sees the right `XIPropertyCreated` / `XIPropertyModified`
/// classification per xiproperty.c:798-800.
///
/// # Errors
/// * [`XiPropError::BadValue`] — `format` not in {8, 16, 32}.
/// * [`XiPropError::BadMatch`] — Prepend/Append onto a property whose
///   stored `format`/`type` differs from the request.
pub fn apply_change_property(
    device: &mut XiDevice,
    mode: u8,
    format: u8,
    property: AtomId,
    type_atom: AtomId,
    data: &[u8],
) -> Result<PropWhat, XiPropError> {
    if format != 8 && format != 16 && format != 32 {
        return Err(XiPropError::BadValue);
    }

    match device.properties.get_mut(&property) {
        None => {
            // New property: always a Replace (xiproperty.c:700-706).
            device.properties.insert(
                property,
                XiProperty {
                    type_atom,
                    format,
                    data: data.to_vec(),
                    read_only: false,
                    deletable: true,
                },
            );
            Ok(PropWhat::Created)
        }
        Some(existing) => {
            if mode != XI_PROP_MODE_REPLACE {
                // Append/Prepend require matching format+type
                // (xiproperty.c:714-717).
                if existing.format != format || existing.type_atom != type_atom {
                    return Err(XiPropError::BadMatch);
                }
            }
            match mode {
                XI_PROP_MODE_PREPEND => {
                    let mut new_data = Vec::with_capacity(data.len() + existing.data.len());
                    new_data.extend_from_slice(data);
                    new_data.extend_from_slice(&existing.data);
                    existing.data = new_data;
                }
                XI_PROP_MODE_APPEND => {
                    existing.data.extend_from_slice(data);
                }
                // Replace (mode 0) or any other value: overwrite. xserver
                // validates mode in check_change_property; the dispatch
                // arm rejects bad modes as BadValue before calling here.
                _ => {
                    existing.type_atom = type_atom;
                    existing.format = format;
                    existing.data = data.to_vec();
                }
            }
            Ok(PropWhat::Modified)
        }
    }
}

/// Apply an `XIDeleteProperty` request: remove the property if present.
///
/// No error if absent (xserver only errors on a bad atom, which we
/// cannot detect here; the dispatch layer owns atom validity).
/// Mirrors `XIDeleteDeviceProperty()` (xiproperty.c:644-...).
///
/// Returns `Some(PropWhat::Deleted)` if a property was actually removed
/// (so the caller can emit `XI_PropertyEvent` / `DevicePropertyNotify`),
/// or `None` when the property was already absent.
#[must_use]
pub fn apply_delete_property(device: &mut XiDevice, property: AtomId) -> Option<PropWhat> {
    device
        .properties
        .remove(&property)
        .map(|_| PropWhat::Deleted)
}

/// Encode an `XI_PropertyEvent` (XI2 evtype 12).
///
/// Wire layout (`xXIPropertyEvent`, `XI2proto.h:1049-1066`, 32 bytes):
/// ```text
///   0   type        = 35 (GenericEvent)
///   1   extension   = XI major opcode (137 for this server)
///   2   sequenceNumber (CARD16)
///   4   length         (CARD32, in 4-byte units — 0 for this event)
///   8   evtype      = 12 (XI_PropertyEvent)
///  10   deviceid       (CARD16)
///  12   time           (Time = CARD32)
///  16   property       (Atom = CARD32)
///  20   what           (CARD8: 0=Deleted, 1=Created, 2=Modified)
///  21   pad0           (CARD8)
///  22   pad1           (CARD16)
///  24   pad2           (CARD32)
///  28   pad3           (CARD32)
/// ```
///
/// `length` is always 0 — `xXIPropertyEvent` has no trailing payload
/// (clients re-query the value).
#[must_use]
pub fn encode_xi2_property_event(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    xi_major: u8,
    deviceid: u16,
    time: u32,
    property: AtomId,
    what: PropWhat,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(35); // GenericEvent
    out.push(xi_major); // XI extension major opcode
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0); // length = 0
    write_u16(byte_order, &mut out, 12); // evtype = XI_PropertyEvent
    write_u16(byte_order, &mut out, deviceid);
    write_u32(byte_order, &mut out, time);
    write_u32(byte_order, &mut out, property.0);
    out.push(what as u8);
    out.extend_from_slice(&[0u8; 11]); // bytes 21-31: pad0..pad3
    debug_assert_eq!(out.len(), 32);
    out
}

// ---------------------------------------------------------------------------
// XI 1.x device-property encoders
//
// These mirror the XI2 encoders above but use the XInput-1 wire layouts
// (XIproto.h), which differ in two ways:
//   * the request device id is a CARD8 (XI2 uses CARD16) — handled by the
//     dispatch layer, which passes a `deviceid: u16` into these encoders;
//   * the replies carry an explicit `deviceid` field (the XI2 replies do
//     not). The earlier zero-stub wrongly returned device=0; MATE's
//     settings daemon reads it back, so it must echo the requested id.
// ---------------------------------------------------------------------------

/// Encode an XI 1.x `ListDeviceProperties` reply.
///
/// Layout (`xListDevicePropertiesReply`, XIproto.h:1442-1454, 32-byte
/// header):
/// ```text
///   0   repType = 1 (X_Reply)
///   1   RepType  (pad in our encoder; the major opcode is not echoed)
///   2   sequenceNumber (CARD16)
///   4   length   = nAtoms (one CARD32 atom each → 4-byte units == count)
///   8   nAtoms   (CARD16)
///  10   pad1     (CARD16)
///  12   pad2..pad6 (20 bytes)
///  32   nAtoms * Atom (CARD32) follows
/// ```
///
/// Note: unlike the XI2 `xXIListPropertiesReply`, the XI 1.x reply has NO
/// `deviceid` field — the device id appears only in the request. So this
/// is byte-identical to [`encode_list_properties_reply`]; it exists as a
/// distinct entry point for clarity and to document the XIproto source.
#[must_use]
pub fn encode_xi1_list_properties_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    device: &XiDevice,
) -> Vec<u8> {
    let atoms: Vec<AtomId> = device.properties.keys().copied().collect();
    #[allow(clippy::cast_possible_truncation)]
    let num = atoms.len() as u16;

    let mut reply = fixed_header(byte_order, sequence, u32::from(num));
    write_u16(byte_order, &mut reply, num); // bytes 8-9: nAtoms
    reply.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad1..pad6
    debug_assert_eq!(reply.len(), 32);
    for atom in atoms {
        write_u32(byte_order, &mut reply, atom.0);
    }
    reply
}

/// Encode an XI 1.x `GetDeviceProperty` reply and (if requested + fully
/// consumed) delete the property from `device`.
///
/// Layout (`xGetDevicePropertyReply`, XIproto.h:1514-1527, 32-byte
/// header):
/// ```text
///   0   repType = 1     12  bytesAfter (CARD32)
///   1   RepType (pad)   16  nItems (CARD32)
///   2   sequenceNumber  20  format (CARD8)
///   4   length          21  deviceid (CARD8)  <-- XI1-specific field
///   8   propertyType    22  pad1 (CARD16)
///       (Atom/CARD32)   24  pad2 (CARD32)
///                       28  pad3 (CARD32)
///   32  value bytes (length bytes, then pad to 4)
/// ```
/// `length` (bytes 4-7) is the value length in 4-byte units.
///
/// The semantics (absent → None/0/0; type-mismatch → metadata-only;
/// value window via offset/len in 4-byte units; delete-after-read gate)
/// are shared with the XI2 path through [`compute_get_property`]. The
/// only wire difference from `xXIGetPropertyReply` is the `deviceid`
/// byte at offset 21 (XI2 has a pad there).
///
/// Cross-checked against the working Xorg xtrace (mate-xorg-mbp.xtrace):
/// absent → `:32: type=None bytesAfter=0 device=N`; present u8 INTEGER →
/// `:36: type=INTEGER bytesAfter=0 device=N value=0xVV`.
///
/// # Errors
/// Returns [`XiPropError::BadValue`] when `offset` lies past the end of
/// the stored value.
pub fn encode_xi1_get_property_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    device: &mut XiDevice,
    property: AtomId,
    requested_type: AtomId,
    offset: u32,
    len: u32,
    delete: bool,
) -> Result<Vec<u8>, XiPropError> {
    let device_id = device.id;
    let result = compute_get_property(
        device.properties.get(&property),
        requested_type,
        offset,
        len,
    )
    .ok_or(XiPropError::BadValue)?;

    let length_field = u32::try_from(result.data.len().div_ceil(4)).unwrap_or(0);

    let mut reply = fixed_header(byte_order, sequence, length_field);
    write_u32(byte_order, &mut reply, result.type_atom.0); // bytes 8-11
    write_u32(byte_order, &mut reply, result.bytes_after); // bytes 12-15
    write_u32(byte_order, &mut reply, result.num_items); // bytes 16-19
    reply.push(result.format); // byte 20: format
    #[allow(clippy::cast_possible_truncation)]
    reply.push(device_id as u8); // byte 21: deviceid (CARD8, XI1-specific)
    reply.extend_from_slice(&[0u8; 10]); // bytes 22-31: pad1..pad3
    debug_assert_eq!(reply.len(), 32);

    match (byte_order, result.format) {
        (ClientByteOrder::BigEndian, 16) => {
            for value in result.data.chunks_exact(2) {
                reply.extend_from_slice(&[value[1], value[0]]);
            }
            reply.extend_from_slice(result.data.chunks_exact(2).remainder());
        }
        (ClientByteOrder::BigEndian, 32) => {
            for value in result.data.chunks_exact(4) {
                reply.extend_from_slice(&[value[3], value[2], value[1], value[0]]);
            }
            reply.extend_from_slice(result.data.chunks_exact(4).remainder());
        }
        _ => reply.extend_from_slice(&result.data),
    }
    pad_to_4(&mut reply);

    if delete && result.bytes_after == 0 {
        device.properties.remove(&property);
    }

    Ok(reply)
}

/// Encode an XI 1.x `DevicePropertyNotify` event (XI1 event code 16).
///
/// Wire layout (`devicePropertyNotify`, XIproto.h, 32 bytes):
/// ```text
///   0   type           = first_event + XI_DevicePropertyNotify(=16)
///   1   state          = 0 (PropertyNewValue) | 1 (PropertyDelete)
///   2   sequenceNumber (CARD16)
///   4   time           (Time = CARD32)
///   8   atom           (Atom = CARD32)
///  12   pad0           (CARD32)
///  16   pad1           (CARD32)
///  20   pad2           (CARD32)
///  24   pad3           (CARD32)
///  28   pad5           (CARD16)
///  30   pad4           (CARD8)
///  31   deviceid       (CARD8)   <-- LAST byte (XIproto.h "deviceid" field)
/// ```
///
/// There is NO window field on the wire: `XDevicePropertyNotifyEvent` in
/// Xlib exposes a `window` member, but it is not part of the protocol
/// struct — Xlib hard-codes the application's selection window when
/// translating, so the wire form omits it entirely.
///
/// `state` is the XI1 analogue of the XI2 `what` byte:
///   * `Created` (XI2 1) and `Modified` (XI2 2) both map to
///     `PropertyNewValue` (XI1 0); XI1 has no "first appearance" hint.
///   * `Deleted`  (XI2 0) maps to `PropertyDelete` (XI1 1).
///
/// `deviceid` is a `CARD8` here (XI1 device ids are 8-bit on the wire),
/// truncated from the `u16` the dispatch layer carries.
#[must_use]
pub fn encode_xi1_device_property_notify(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    first_event: u8,
    deviceid: u16,
    time: u32,
    property: AtomId,
    deleted: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(first_event + XI_DEVICE_PROPERTY_NOTIFY_OFFSET); // byte 0: type
    out.push(u8::from(deleted)); // byte 1: state (0=NewValue / 1=Delete)
    write_u16(byte_order, &mut out, sequence.0); // bytes 2-3
    write_u32(byte_order, &mut out, time); // bytes 4-7
    write_u32(byte_order, &mut out, property.0); // bytes 8-11
    out.extend_from_slice(&[0u8; 19]); // bytes 12-30: pad0..pad5 + pad4
    #[allow(clippy::cast_possible_truncation)]
    out.push(deviceid as u8); // byte 31: deviceid (CARD8, last byte)
    debug_assert_eq!(out.len(), 32);
    out
}

// --- small wire helpers (kept local to avoid pulling private protocol fns) ---

/// Build the 8-byte reply prefix shared by all XI2 replies:
/// `repType=1`, `RepType=0` pad byte, `sequenceNumber`, `length`.
/// Matches `x11::fixed_reply` in the protocol crate.
fn fixed_header(byte_order: ClientByteOrder, sequence: SequenceNumber, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1); // repType = X_Reply
    out.push(0); // RepType pad (matches existing XI2 arms)
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
    out
}

fn write_u16(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u16) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

fn write_u32(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: u32) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

fn write_i16(byte_order: ClientByteOrder, out: &mut Vec<u8>, value: i16) {
    match byte_order {
        ClientByteOrder::LittleEndian => out.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => out.extend_from_slice(&value.to_be_bytes()),
    }
}

/// Encode an XI 1.x `deviceKeyButtonPointer` wire event (XIproto.h):
/// DeviceKeyPress/Release, DeviceButtonPress/Release and
/// DeviceMotionNotify all share this 32-byte layout. `event_type` is
/// the absolute wire code (`first_event + offset`). The deviceid top
/// bit (MORE_EVENTS) stays clear — we never append deviceValuator
/// follow-ups.
#[allow(clippy::too_many_arguments)]
pub fn encode_xi1_device_input_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    detail: u8,
    sequence: SequenceNumber,
    time: u32,
    root: u32,
    event_window: u32,
    child: u32,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    state: u16,
    deviceid: u16,
) {
    out.push(event_type); // byte 0: type
    out.push(detail); // byte 1: detail (button / keycode)
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    write_u32(byte_order, out, time); // bytes 4-7
    write_u32(byte_order, out, root); // bytes 8-11
    write_u32(byte_order, out, event_window); // bytes 12-15
    write_u32(byte_order, out, child); // bytes 16-19
    write_i16(byte_order, out, root_x); // bytes 20-21
    write_i16(byte_order, out, root_y); // bytes 22-23
    write_i16(byte_order, out, event_x); // bytes 24-25
    write_i16(byte_order, out, event_y); // bytes 26-27
    write_u16(byte_order, out, state); // bytes 28-29
    out.push(1); // byte 30: same_screen = true
    #[allow(clippy::cast_possible_truncation)]
    out.push((deviceid as u8) & 0x7f); // byte 31: deviceid, MORE_EVENTS clear
}

/// Encode an XI 1.x `deviceFocus` wire event (XIproto.h:1590-1603):
/// DeviceFocusIn and DeviceFocusOut share this 32-byte layout.
/// `event_type` is the absolute wire code (`first_event + 6/7`);
/// `detail` is a `Notify*` constant; `mode` is NotifyNormal /
/// NotifyGrab / NotifyUngrab / NotifyWhileGrabbed.
/// ```text
///   0  type      4  time (CARD32)   12  mode (BYTE)
///   1  detail    8  window (CARD32) 13  deviceid (CARD8)
///   2  sequence                     14  pad (18 bytes)
/// ```
#[allow(clippy::too_many_arguments)]
pub fn encode_xi1_device_focus_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    detail: u8,
    sequence: SequenceNumber,
    time: u32,
    window: u32,
    mode: u8,
    deviceid: u16,
) {
    out.push(event_type); // byte 0: type
    out.push(detail); // byte 1: detail (Notify*)
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    write_u32(byte_order, out, time); // bytes 4-7
    write_u32(byte_order, out, window); // bytes 8-11
    out.push(mode); // byte 12: mode
    #[allow(clippy::cast_possible_truncation)]
    out.push(deviceid as u8); // byte 13: deviceid (CARD8)
    out.extend_from_slice(&[0u8; 18]); // bytes 14-31: pad
}

/// XI 1.x `deviceMappingNotify` wire event (XIproto.h:1635-1655).
/// Fired when SetDeviceModifierMapping / SetDeviceButtonMapping /
/// ChangeDeviceKeyMapping changes the device's mapping state.
///
/// `request` is the kind of mapping that changed (XInput.h):
///   0 = MappingModifier, 1 = MappingKeyboard, 2 = MappingPointer.
/// `first_keycode` and `count` are only meaningful for MappingKeyboard
/// (the keysym range that was rewritten); zero for the other two.
pub fn encode_xi1_device_mapping_notify(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    deviceid: u8,
    sequence: SequenceNumber,
    time: u32,
    request: u8,
    first_keycode: u8,
    count: u8,
) {
    // Wire layout per XIproto.h `deviceMappingNotify`:
    //   byte 0: type
    //   byte 1: deviceid
    //   bytes 2..4: sequenceNumber
    //   byte 4: request   (MappingModifier=0 / MappingKeyboard=1 / MappingPointer=2)
    //   byte 5: firstKeyCode
    //   byte 6: count
    //   byte 7: pad1
    //   bytes 8..12: time
    //   bytes 12..32: 5 × CARD32 pad
    out.push(event_type); // byte 0
    out.push(deviceid); // byte 1
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    out.push(request); // byte 4
    out.push(first_keycode); // byte 5
    out.push(count); // byte 6
    out.push(0); // byte 7: pad1
    write_u32(byte_order, out, time); // bytes 8-11
    out.extend_from_slice(&[0u8; 20]); // bytes 12-31: 5×u32 pad
}

/// XI 1.x `changeDeviceNotify` wire event (XIproto.h:1660-1680).
/// Fired when ChangeKeyboardDevice / ChangePointerDevice rebinds the
/// device that backs the core pointer / keyboard.
///
/// `request` is the device kind that changed (XInput.h):
///   0 = NewPointer, 1 = NewKeyboard.
pub fn encode_xi1_change_device_notify(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    deviceid: u8,
    sequence: SequenceNumber,
    time: u32,
    request: u8,
) {
    out.push(event_type); // byte 0: type
    out.push(deviceid); // byte 1: deviceid
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    write_u32(byte_order, out, time); // bytes 4-7
    out.push(request); // byte 8: request kind
    out.extend_from_slice(&[0u8; 23]); // bytes 9-31: pad
}

/// XI 1.x `deviceStateNotify` wire event (XIproto.h:1615-1630). The
/// first 32-byte event of a device-state chain: up to 32 button bits,
/// 32 key bits and 3 valuator values; continuation events (key state /
/// button state / deviceValuator) follow when `deviceid` carries
/// `XI1_MORE_EVENTS`.
///
/// `classes_reported` packs the class bits (`1 << KeyClass(0)` /
/// `ButtonClass(1)` / `ValuatorClass(2)`) in the low 6 bits and the
/// valuator mode (Relative=0 / Absolute=1) at `ModeBitsShift` (= 6),
/// per XIproto.h:70-71 — libXi reads `num_classes` and the per-class
/// blocks straight out of these fields (XExtInt.c `XI_DeviceStateNotify`).
#[allow(clippy::too_many_arguments)]
pub fn encode_xi1_device_state_notify(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    deviceid: u8,
    sequence: SequenceNumber,
    time: u32,
    num_keys: u8,
    num_buttons: u8,
    num_valuators: u8,
    classes_reported: u8,
    buttons: [u8; 4],
    keys: [u8; 4],
    valuators: [i32; 3],
) {
    out.push(event_type); // byte 0: type
    out.push(deviceid); // byte 1: deviceid (| XI1_MORE_EVENTS)
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    write_u32(byte_order, out, time); // bytes 4-7
    out.push(num_keys); // byte 8
    out.push(num_buttons); // byte 9
    out.push(num_valuators); // byte 10
    out.push(classes_reported); // byte 11
    out.extend_from_slice(&buttons); // bytes 12-15
    out.extend_from_slice(&keys); // bytes 16-19
    for v in valuators {
        write_u32(byte_order, out, v.cast_unsigned()); // bytes 20-31
    }
}

/// XI 1.x `deviceKeyStateNotify` continuation (XIproto.h:1638-1644):
/// key-down bits for keycodes 32..=255 (the first 32 ride in the
/// leading deviceStateNotify). libXi memcpys these 28 bytes into
/// `XKeyStatus.keys[4..]` and forces `num_keys = 256`.
pub fn encode_xi1_device_key_state_notify(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    deviceid: u8,
    sequence: SequenceNumber,
    keys: &[u8; 28],
) {
    out.push(event_type); // byte 0: type
    out.push(deviceid); // byte 1: deviceid (| XI1_MORE_EVENTS)
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    out.extend_from_slice(keys); // bytes 4-31
}

/// XI 1.x `deviceValuator` event (XIproto.h:1538-1552). As a
/// DeviceStateNotify continuation it carries valuators
/// `first_valuator..first_valuator+num_valuators` (libXi appends at
/// most 3 per continuation onto `XValuatorStatus.valuators`).
#[allow(clippy::too_many_arguments)]
pub fn encode_xi1_device_valuator(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    event_type: u8,
    deviceid: u8,
    sequence: SequenceNumber,
    device_state: u16,
    num_valuators: u8,
    first_valuator: u8,
    valuators: [i32; 6],
) {
    out.push(event_type); // byte 0: type
    out.push(deviceid); // byte 1: deviceid (| XI1_MORE_EVENTS)
    write_u16(byte_order, out, sequence.0); // bytes 2-3
    write_u16(byte_order, out, device_state); // bytes 4-5: KeyButMask
    out.push(num_valuators); // byte 6
    out.push(first_valuator); // byte 7
    for v in valuators {
        write_u32(byte_order, out, v.cast_unsigned()); // bytes 8-31
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::AtomTable;

    fn touchpad_info(tap_enabled: bool) -> DeviceInfo {
        use crate::core_loop::message::{BoolSetting, LibinputConfigSnapshot};
        DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "SynPS/2 Synaptics TouchPad".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0x046d,
            product_id: 0xc52f,
            is_touchpad: true,
            config: LibinputConfigSnapshot {
                tap: BoolSetting {
                    available: true,
                    current: tap_enabled,
                    default: false,
                },
                natural_scroll: BoolSetting {
                    available: true,
                    current: false,
                    default: true,
                },
                dwt: BoolSetting {
                    available: true,
                    current: true,
                    default: true,
                },
                ..Default::default()
            },
        }
    }

    fn mouse_info() -> DeviceInfo {
        use crate::core_loop::message::LibinputConfigSnapshot;
        DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "USB Mouse".into(),
            device_node: "/dev/input/event1".into(),
            sysname: "event1".into(),
            vendor_id: 0x046d,
            product_id: 0xc52f,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        }
    }

    /// A plain mouse whose libinput snapshot reports the knobs a real
    /// relative pointer exposes (accel + accel-profile + left-handed +
    /// natural-scroll + middle-emulation + send-events) but NOT tapping.
    /// Mirrors what `libinput_config::gather` yields for a mouse and what
    /// Xorg+libinput exposes (per the on-HW xtrace).
    fn mouse_info_with_accel() -> DeviceInfo {
        use crate::core_loop::message::{
            BitFlags2, BoolSetting, FloatSetting, LibinputConfigSnapshot, OneHot2,
        };
        let avail_bool = BoolSetting {
            available: true,
            current: false,
            default: false,
        };
        let config = LibinputConfigSnapshot {
            accel: FloatSetting {
                available: true,
                current: 0.0,
                default: 0.0,
            },
            accel_profile: OneHot2 {
                available: true,
                current: Some(0),
                default: Some(0),
            },
            accel_profile_available_mask: 0b011,
            left_handed: avail_bool,
            natural_scroll: avail_bool,
            middle_emulation: avail_bool,
            send_events: BitFlags2 {
                available_mask: 0b01,
                current_mask: 0,
                default_mask: 0,
            },
            ..Default::default()
        };
        DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "USB Mouse".into(),
            device_node: "/dev/input/event1".into(),
            sysname: "event1".into(),
            vendor_id: 0x046d,
            product_id: 0xc52f,
            is_touchpad: false,
            config,
        }
    }

    #[test]
    fn initial_devices_has_four_entries() {
        let devs = initial_xi_devices();
        assert_eq!(devs.len(), 4);
        assert!(devs.iter().any(|d| d.id == DEVICEID_MASTER_POINTER));
        assert!(devs.iter().any(|d| d.id == DEVICEID_MASTER_KEYBOARD));
        assert!(devs.iter().any(|d| d.id == DEVICEID_XTEST_POINTER));
        assert!(devs.iter().any(|d| d.id == DEVICEID_XTEST_KEYBOARD));
    }

    #[test]
    fn initial_names_match_xiquerydevice_handler() {
        let devs = initial_xi_devices();
        let get = |id: u16| devs.iter().find(|d| d.id == id).unwrap().name.as_str();
        assert_eq!(get(DEVICEID_MASTER_POINTER), "Virtual core pointer");
        assert_eq!(get(DEVICEID_MASTER_KEYBOARD), "Virtual core keyboard");
        assert_eq!(get(DEVICEID_XTEST_POINTER), "Virtual core XTEST pointer");
        assert_eq!(get(DEVICEID_XTEST_KEYBOARD), "Virtual core XTEST keyboard");
    }

    #[test]
    fn initial_devices_have_empty_property_maps() {
        let devs = initial_xi_devices();
        for d in &devs {
            assert!(
                d.properties.is_empty(),
                "device {} had properties at init",
                d.id
            );
        }
    }

    #[test]
    fn seed_pointer_properties_sets_name_and_properties() {
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let info = touchpad_info(true);
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        assert_eq!(slave.name, "SynPS/2 Synaptics TouchPad");

        // Tap enabled — INTEGER/8/[1]
        let tap_atom = atoms.intern(PROP_TAPPING_ENABLED, false);
        let tap_prop = slave.properties.get(&tap_atom).unwrap();
        assert_eq!(tap_prop.type_atom, XA_INTEGER);
        assert_eq!(tap_prop.format, 8);
        assert_eq!(tap_prop.data, vec![1u8]);

        // Tap default — INTEGER/8/[0]
        let tap_def_atom = atoms.intern("libinput Tapping Enabled Default", false);
        let tap_def = slave.properties.get(&tap_def_atom).unwrap();
        assert_eq!(tap_def.data, vec![0u8]);

        // Natural scroll enabled — INTEGER/8/[0]
        let ns_atom = atoms.intern("libinput Natural Scrolling Enabled", false);
        let ns_prop = slave.properties.get(&ns_atom).unwrap();
        assert_eq!(ns_prop.data, vec![0u8]);

        // Natural scroll default — INTEGER/8/[1]
        let ns_def_atom = atoms.intern("libinput Natural Scrolling Enabled Default", false);
        let ns_def = slave.properties.get(&ns_def_atom).unwrap();
        assert_eq!(ns_def.data, vec![1u8]);

        // DWT enabled — INTEGER/8/[1]
        let dwt_atom = atoms.intern("libinput Disable While Typing Enabled", false);
        let dwt_prop = slave.properties.get(&dwt_atom).unwrap();
        assert_eq!(dwt_prop.data, vec![1u8]);

        // DWT default — INTEGER/8/[1]
        let dwt_def_atom = atoms.intern("libinput Disable While Typing Enabled Default", false);
        let dwt_def = slave.properties.get(&dwt_def_atom).unwrap();
        assert_eq!(dwt_def.data, vec![1u8]);

        // Device Node — STRING/8/raw bytes
        let node_atom = atoms.intern(PROP_DEVICE_NODE, false);
        let node_prop = slave.properties.get(&node_atom).unwrap();
        assert_eq!(node_prop.type_atom, XA_STRING);
        assert_eq!(node_prop.format, 8);
        assert_eq!(node_prop.data, b"/dev/input/event4".to_vec());

        // Device Product ID — INTEGER/32/[vendor_le, product_le]
        let pid_atom = atoms.intern(PROP_DEVICE_PRODUCT_ID, false);
        let pid_prop = slave.properties.get(&pid_atom).unwrap();
        assert_eq!(pid_prop.type_atom, XA_INTEGER);
        assert_eq!(pid_prop.format, 32);
        let mut expected = Vec::new();
        expected.extend_from_slice(&0x046du32.to_le_bytes());
        expected.extend_from_slice(&0xc52fu32.to_le_bytes());
        assert_eq!(pid_prop.data, expected);
    }

    #[test]
    fn seed_pointer_tap_disabled_encodes_zero() {
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let info = touchpad_info(false);
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        let tap_atom = atoms.intern(PROP_TAPPING_ENABLED, false);
        assert_eq!(slave.properties[&tap_atom].data, vec![0u8]);
    }

    #[test]
    fn other_devices_unaffected_by_source_registration() {
        let mut state = crate::server::ServerState::new();
        state.xi_register_source(&touchpad_info(true));
        // Master pointer/keyboard, XTEST pointer/keyboard each keep their
        // independent ordinary property maps.
        for id in [
            DEVICEID_MASTER_POINTER,
            DEVICEID_MASTER_KEYBOARD,
            DEVICEID_XTEST_POINTER,
            DEVICEID_XTEST_KEYBOARD,
        ] {
            let dev = state.xi_devices.device(id).unwrap();
            if id == DEVICEID_XTEST_POINTER || id == DEVICEID_XTEST_KEYBOARD {
                assert_eq!(dev.properties.len(), 1, "XTEST marker remains on {id}");
                assert!(dev.properties.contains_key(&state.xtest_device_atom));
            } else {
                assert!(dev.properties.is_empty(), "device {id} got properties");
            }
        }
    }

    #[test]
    fn unregister_source_removes_its_facets_and_properties() {
        let mut state = crate::server::ServerState::new();
        let info = touchpad_info(true);
        let id = state.xi_register_source(&info)[0];
        let slave = state.xi_devices.device(id).unwrap();
        assert!(!slave.properties.is_empty());
        assert_eq!(slave.device_node.as_deref(), Some("/dev/input/event4"));

        state.xi_unregister_source(info.source_id);
        assert!(state.xi_devices.device(id).is_none());
        assert_eq!(
            state
                .xi_devices
                .device(DEVICEID_XTEST_POINTER)
                .unwrap()
                .name,
            registry::NAME_XTEST_POINTER
        );
    }

    #[test]
    fn unregister_source_is_idempotent() {
        let mut state = crate::server::ServerState::new();
        let info = touchpad_info(true);
        let id = state.xi_register_source(&info)[0];
        state.xi_unregister_source(info.source_id);
        assert!(state.xi_unregister_source(info.source_id).is_empty());
        assert!(state.xi_devices.device(id).is_none());
    }

    #[test]
    fn seed_second_touchpad_keeps_first_source_independent() {
        let mut state = crate::server::ServerState::new();
        let info1 = touchpad_info(true);
        let mut info2 = touchpad_info(false);
        info2.source_id = InputSourceId(info1.source_id.0 + 1);
        info2.name = "ETPS/2 Elantech Touchpad".into();
        info2.device_node = "/dev/input/event5".into();
        let id1 = state.xi_register_source(&info1)[0];
        let id2 = state.xi_register_source(&info2)[0];
        let first = state.xi_devices.device(id1).unwrap();
        let second = state.xi_devices.device(id2).unwrap();
        assert_eq!(first.name, "SynPS/2 Synaptics TouchPad");
        assert_eq!(first.device_node.as_deref(), Some("/dev/input/event4"));
        assert_eq!(second.name, "ETPS/2 Elantech Touchpad");
        assert_eq!(second.device_node.as_deref(), Some("/dev/input/event5"));
        let node_atom = state.atoms.intern(PROP_DEVICE_NODE, false);
        assert_eq!(
            first.properties[&node_atom].data,
            b"/dev/input/event4".to_vec()
        );
        assert_eq!(
            second.properties[&node_atom].data,
            b"/dev/input/event5".to_vec()
        );
    }

    #[test]
    fn seed_pointer_properties_uses_source_capability_not_touchpad_flag() {
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let info = mouse_info();
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        assert_eq!(slave.name, "USB Mouse");
        assert_eq!(slave.properties.len(), 2);
    }

    #[test]
    fn source_registration_publishes_mouse_without_accel_settings() {
        let mut state = crate::server::ServerState::new();
        let info = mouse_info();
        let ids = state.xi_register_source(&info);
        assert_eq!(ids.len(), 1, "pointer capability publishes a facet");
        assert!(state.xi_devices.device(ids[0]).is_some());
        let slave = state.xi_devices.device(DEVICEID_XTEST_POINTER).unwrap();
        assert_eq!(slave.name, registry::NAME_XTEST_POINTER);
        let marker = state.xtest_device_atom;
        assert_eq!(
            slave.properties.get(&marker),
            Some(&XiProperty {
                type_atom: XA_INTEGER,
                format: 8,
                data: vec![1],
                read_only: true,
                deletable: false,
            })
        );
    }

    /// A configurable mouse (a pointer that is NOT a touchpad, e.g. accel
    /// available) must have its libinput properties seeded — this is the
    /// `libinput Accel Speed` the KDE Mouse KCM reads. The device must NOT
    /// be flagged `is_touchpad`, and touchpad-only props (Tapping) must not
    /// appear. Regression guard for the kcm_mouse SIGSEGV (missing
    /// `libinput Accel Speed` atom → InternAtom None → client null-deref).
    #[test]
    fn seed_pointer_mouse_seeds_accel_and_keeps_mouse_flag() {
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let info = mouse_info_with_accel();
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        // A mouse is not a touchpad, even though it now carries libinput props.
        assert!(!slave.is_touchpad, "mouse must not be flagged is_touchpad");
        // The accel property the Mouse KCM reads must be present.
        let accel = atoms.intern("libinput Accel Speed", true);
        assert!(
            slave.properties.contains_key(&accel),
            "libinput Accel Speed must be seeded on a configurable mouse"
        );
        // Touchpad-only props must NOT appear (tap unavailable on a mouse).
        let tap = atoms.intern("libinput Tapping Enabled", true);
        assert!(
            !slave.properties.contains_key(&tap),
            "Tapping must not be seeded on a mouse (tap unavailable)"
        );
    }

    /// N6 (review round): the *seeded* width of `libinput Accel Profile
    /// Enabled` comes from the hardcoded `encode_onehot(.., 3)` literal
    /// in `descriptor_current_data`, entirely independent of the
    /// write-side `ValueKind::OneHotOrNone { n: 3, min: 3 }` in the descriptor
    /// table (pinned separately by `accel_profile_enabled_is_three_wide`
    /// in `libinput_props.rs`). Now that the write path (T3) no longer
    /// enforces `len == n`, these two constants could drift without
    /// either half's own tests noticing — pin the seeded width
    /// directly, since it's what msd's `nitems_ret >= 2` precondition
    /// actually depends on.
    #[test]
    fn seed_accel_profile_enabled_is_three_bytes() {
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let info = mouse_info_with_accel();
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        let atom = atoms.intern("libinput Accel Profile Enabled", true);
        let prop = slave
            .properties
            .get(&atom)
            .expect("Accel Profile Enabled seeded (accel_profile.available=true)");
        assert_eq!(prop.data.len(), 3, "seeded width is 3 bytes");
    }

    /// Physical pointer seeding must leave virtual XTEST device 4 alone.
    #[test]
    fn source_registration_preserves_xtest_for_configurable_mouse() {
        let mut state = crate::server::ServerState::new();
        let ids = state.xi_register_source(&mouse_info_with_accel());
        assert_eq!(ids, vec![6]);
        let slave = state.xi_devices.device(DEVICEID_XTEST_POINTER).unwrap();
        assert_eq!(slave.name, registry::NAME_XTEST_POINTER);
        assert!(
            !slave
                .properties
                .contains_key(&state.atoms.intern("libinput Accel Speed", true)),
            "physical libinput properties do not belong to XTEST"
        );
        assert!(!slave.is_touchpad);
    }

    /// A pointer source publishes its facet even without available accel settings.
    #[test]
    fn source_registration_publishes_pointer_without_accel_property() {
        use crate::core_loop::message::{BoolSetting, LibinputConfigSnapshot};
        // Has a scroll knob but is NOT a real relative pointer (no accel).
        let config = LibinputConfigSnapshot {
            natural_scroll: BoolSetting {
                available: true,
                current: false,
                default: false,
            },
            ..Default::default()
        };
        let phantom = DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "Keychron K1 Pro Consumer Control".into(),
            device_node: "/dev/input/event5".into(),
            sysname: "event5".into(),
            vendor_id: 0x3434,
            product_id: 0x0311,
            is_touchpad: false,
            config,
        };
        let mut state = crate::server::ServerState::new();
        let first = mouse_info_with_accel();
        let first_id = state.xi_register_source(&first)[0];
        let phantom_id = state.xi_register_source(&phantom)[0];
        assert_ne!(first_id, phantom_id);
        let physical = state.xi_devices.device(phantom_id).unwrap();
        let natural_scroll = state
            .atoms
            .intern("libinput Natural Scrolling Enabled", true);
        assert!(physical.properties.contains_key(&natural_scroll));
        let slave = state.xi_devices.device(DEVICEID_XTEST_POINTER).unwrap();
        assert_eq!(
            slave.name,
            registry::NAME_XTEST_POINTER,
            "phantom must not seize the virtual slot"
        );
        let accel = state.atoms.intern("libinput Accel Speed", true);
        assert!(
            !slave.properties.contains_key(&accel),
            "physical Accel Speed must not be seeded on XTEST"
        );
    }

    /// Two physical sources keep separate descriptor sets and identity values.
    #[test]
    fn independent_touchpad_and_mouse_facets_have_independent_properties() {
        let mut state = crate::server::ServerState::new();
        let info1 = touchpad_info(true);
        let mut info2 = mouse_info_with_accel();
        info2.source_id = InputSourceId(info1.source_id.0 + 1);
        let touchpad_id = state.xi_register_source(&info1)[0];
        let mouse_id = state.xi_register_source(&info2)[0];
        let tap = state.atoms.intern("libinput Tapping Enabled", true);
        let accel = state.atoms.intern("libinput Accel Speed", true);
        assert!(
            state
                .xi_devices
                .device(touchpad_id)
                .unwrap()
                .properties
                .contains_key(&tap)
        );
        assert!(
            state
                .xi_devices
                .device(mouse_id)
                .unwrap()
                .properties
                .contains_key(&accel)
        );
        assert!(
            !state
                .xi_devices
                .device(mouse_id)
                .unwrap()
                .properties
                .contains_key(&tap)
        );
    }

    #[test]
    fn source_registration_preserves_xtest_for_touchpad() {
        let mut state = crate::server::ServerState::new();
        let info = touchpad_info(true);
        let id = state.xi_register_source(&info)[0];
        assert_ne!(id, DEVICEID_XTEST_POINTER);
        let slave = state.xi_devices.device(DEVICEID_XTEST_POINTER).unwrap();
        assert_eq!(slave.name, registry::NAME_XTEST_POINTER);
        assert_eq!(slave.properties.len(), 1);
        assert!(slave.properties.contains_key(&state.xtest_device_atom));
    }

    /// T2: seeding is driven entirely by the descriptor table. Available
    /// props must appear with the snapshot's current value (and a Default
    /// companion with the snapshot's default value); unavailable props
    /// must NOT appear in the property map.
    #[test]
    fn seed_then_table_matches() {
        use crate::core_loop::message::{BoolSetting, LibinputConfigSnapshot};
        let mut atoms = AtomTable::new();
        // Force a known FLOAT atom for the test; the production path
        // uses `state.float_atom`.
        let float_atom = atoms.intern("FLOAT", false);
        let info = DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "SynPS/2 Synaptics TouchPad".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0x046d,
            product_id: 0xc52f,
            is_touchpad: true,
            config: LibinputConfigSnapshot {
                tap: BoolSetting {
                    available: true,
                    current: true,
                    default: false,
                },
                ..Default::default()
            },
        };
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);

        // device_node mirrors the snapshot.
        assert_eq!(slave.device_node.as_deref(), Some("/dev/input/event4"));

        // Tapping Enabled — INTEGER/8/[1].
        let tap = atoms.intern("libinput Tapping Enabled", false);
        let prop = slave.properties.get(&tap).expect("tap prop seeded");
        assert_eq!(prop.type_atom, XA_INTEGER);
        assert_eq!(prop.format, 8);
        assert_eq!(prop.data, vec![1u8]);

        // Tapping Enabled Default — INTEGER/8/[0].
        let tap_def = atoms.intern("libinput Tapping Enabled Default", false);
        let def = slave.properties.get(&tap_def).expect("tap default seeded");
        assert_eq!(def.format, 8);
        assert_eq!(def.data, vec![0u8]);

        // Accel Speed is NOT available in this snapshot — must be absent.
        let accel = atoms.intern("libinput Accel Speed", false);
        assert!(
            !slave.properties.contains_key(&accel),
            "Accel Speed must be absent when accel.available=false"
        );
        let accel_def = atoms.intern("libinput Accel Speed Default", false);
        assert!(
            !slave.properties.contains_key(&accel_def),
            "Accel Speed Default must be absent when accel.available=false"
        );
        let accel_profile = atoms.intern("libinput Accel Profile Enabled", false);
        assert!(
            !slave.properties.contains_key(&accel_profile),
            "Accel Profile Enabled must be absent when accel_profile.available=false"
        );

        // Scroll Methods Available is gated by scroll_method.available_mask
        // != 0 — empty snapshot ⇒ absent.
        let sm_avail = atoms.intern("libinput Scroll Methods Available", false);
        assert!(!slave.properties.contains_key(&sm_avail));
    }

    /// T2: when a setting's `.available_mask` is non-zero, the
    /// `…Available` ReadOnly companion appears with the mask encoded as
    /// one byte per bit.
    #[test]
    fn seed_emits_available_mask_for_groups() {
        use crate::core_loop::message::{LibinputConfigSnapshot, OneHot3};
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let config = LibinputConfigSnapshot {
            scroll_method: OneHot3 {
                available_mask: 0b011, // 2fg + edge, no button
                current: Some(0),
                default: Some(0),
            },
            ..Default::default()
        };
        let info = DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "tp".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: true,
            config,
        };
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        let avail = atoms.intern("libinput Scroll Methods Available", false);
        let p = slave.properties.get(&avail).expect("avail seeded");
        // 3-wide BitFlags, bit0+bit1 set ⇒ [1, 1, 0].
        assert_eq!(p.data, vec![1u8, 1, 0]);
        assert_eq!(p.format, 8);
        assert_eq!(p.type_atom, XA_INTEGER);
    }

    /// T2 regression guard: the Tapping Button Mapping pair has an
    /// irregular default name (`…Mapping Default` instead of
    /// `…Mapping Enabled Default`). When the writable is available, the
    /// default companion MUST also be seeded with the correct 2-byte
    /// one-hot encoding pulled from the snapshot's `.default`.
    #[test]
    fn seed_tap_button_map_default_is_correctly_paired() {
        use crate::core_loop::message::{LibinputConfigSnapshot, OneHot2};
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let config = LibinputConfigSnapshot {
            tap_button_map: OneHot2 {
                available: true,
                current: Some(0), // LRM
                default: Some(1), // LMR
            },
            ..Default::default()
        };
        let info = DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "tp".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: true,
            config,
        };
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);

        // Writable seeded with `current = Some(0)` → [1, 0].
        let cur = atoms.intern("libinput Tapping Button Mapping Enabled", false);
        assert_eq!(slave.properties[&cur].data, vec![1u8, 0]);

        // Default companion seeded with `default = Some(1)` → [0, 1].
        // (Regression: an earlier draft of the seeder stripped " Default"
        // and missed the `…Mapping Default` ↔ `…Mapping Enabled` pair,
        // emitting an empty data block instead.)
        let def = atoms.intern("libinput Tapping Button Mapping Default", false);
        assert_eq!(slave.properties[&def].data, vec![0u8, 1]);
        assert_eq!(slave.properties[&def].format, 8);
    }

    /// T2: Float-typed props (Accel Speed) use the runtime-interned
    /// FLOAT atom for their type, not INTEGER/CARDINAL, and the data is
    /// 4 little-endian IEEE-754 bytes.
    #[test]
    fn seed_accel_speed_uses_float_atom() {
        use crate::core_loop::message::{FloatSetting, LibinputConfigSnapshot};
        let mut atoms = AtomTable::new();
        let float_atom = atoms.intern("FLOAT", false);
        let config = LibinputConfigSnapshot {
            accel: FloatSetting {
                available: true,
                current: 0.25,
                default: 0.0,
            },
            ..Default::default()
        };
        let info = DeviceInfo {
            source_id: crate::xinput::InputSourceId(u64::from(line!())),
            enabled: true,
            resume_key: None,
            capabilities: crate::xinput::InputCapabilities {
                keyboard: false,
                pointer: true,
                touch: false,
            },
            name: "tp".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: true,
            config,
        };
        let mut slave = XiDevice::physical(6, &info, XiFacetKind::PointerTouch);
        seed_pointer_properties(&mut slave, &mut atoms, float_atom, &info);
        let speed = atoms.intern("libinput Accel Speed", false);
        let p = slave.properties.get(&speed).expect("accel speed seeded");
        assert_eq!(p.type_atom, float_atom);
        assert_eq!(p.format, 32);
        assert_eq!(p.data, 0.25f32.to_le_bytes().to_vec());
    }

    // -----------------------------------------------------------------
    // Task 3: XI2 device-property protocol — byte-level tests
    // -----------------------------------------------------------------

    const LE: ClientByteOrder = ClientByteOrder::LittleEndian;
    const SEQ: SequenceNumber = SequenceNumber(0x1234);

    fn u32_le(b: &[u8]) -> u32 {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
    fn u16_le(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }

    /// A device with two known properties:
    ///   atom 100: INTEGER/8/[1] (1 byte)
    ///   atom 200: STRING/8/"abcde" (5 bytes)
    /// plus a 32-bit one at atom 300: INTEGER/32/[0x11223344, 0x55667788].
    fn dev_with_props() -> XiDevice {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        dev.properties.insert(
            AtomId(100),
            XiProperty {
                type_atom: XA_INTEGER,
                format: 8,
                data: vec![1],
                read_only: false,
                deletable: true,
            },
        );
        dev.properties.insert(
            AtomId(200),
            XiProperty {
                type_atom: XA_STRING,
                format: 8,
                data: b"abcde".to_vec(),
                read_only: false,
                deletable: true,
            },
        );
        let mut pid = Vec::new();
        pid.extend_from_slice(&0x1122_3344u32.to_le_bytes());
        pid.extend_from_slice(&0x5566_7788u32.to_le_bytes());
        dev.properties.insert(
            AtomId(300),
            XiProperty {
                type_atom: XA_INTEGER,
                format: 32,
                data: pid,
                read_only: false,
                deletable: true,
            },
        );
        dev
    }

    #[test]
    fn list_properties_bytes() {
        let dev = dev_with_props();
        let reply = encode_list_properties_reply(LE, SEQ, &dev);
        // 32-byte header + 3 atoms * 4 = 44 bytes.
        assert_eq!(reply.len(), 44);
        assert_eq!(reply[0], 1, "repType = X_Reply");
        assert_eq!(u16_le(&reply[2..4]), 0x1234, "sequence");
        // length (bytes 4-7) = num_properties = 3.
        assert_eq!(u32_le(&reply[4..8]), 3);
        // num_properties (bytes 8-9) = 3.
        assert_eq!(u16_le(&reply[8..10]), 3);
        // pad 10..32 must be zero.
        assert!(reply[10..32].iter().all(|&b| b == 0));
        // Atoms in BTreeMap order: 100, 200, 300.
        assert_eq!(u32_le(&reply[32..36]), 100);
        assert_eq!(u32_le(&reply[36..40]), 200);
        assert_eq!(u32_le(&reply[40..44]), 300);
    }

    #[test]
    fn list_properties_empty_device() {
        let dev = XiDevice::new(DEVICEID_MASTER_POINTER, NAME_MASTER_POINTER);
        let reply = encode_list_properties_reply(LE, SEQ, &dev);
        assert_eq!(reply.len(), 32);
        assert_eq!(u32_le(&reply[4..8]), 0);
        assert_eq!(u16_le(&reply[8..10]), 0);
    }

    #[test]
    fn get_property_absent_returns_none_type() {
        let mut dev = dev_with_props();
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(999), AtomId(0), 0, 100, false)
                .unwrap();
        assert_eq!(reply.len(), 32, "no data for absent property");
        assert_eq!(reply[0], 1);
        assert_eq!(u32_le(&reply[4..8]), 0, "length = 0");
        assert_eq!(u32_le(&reply[8..12]), 0, "type = None");
        assert_eq!(u32_le(&reply[12..16]), 0, "bytes_after = 0");
        assert_eq!(u32_le(&reply[16..20]), 0, "num_items = 0");
        assert_eq!(reply[20], 0, "format = 0");
    }

    #[test]
    fn get_property_type_mismatch_returns_metadata_no_data() {
        // atom 200 is STRING; ask for INTEGER → mismatch path.
        let mut dev = dev_with_props();
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(200), XA_INTEGER, 0, 100, false)
                .unwrap();
        assert_eq!(reply.len(), 32, "no value bytes on mismatch");
        assert_eq!(u32_le(&reply[8..12]), XA_STRING.0, "type = stored type");
        // bytes_after = item count = 5 (format 8 → 5 items).
        assert_eq!(u32_le(&reply[12..16]), 5);
        assert_eq!(u32_le(&reply[16..20]), 0, "num_items = 0");
        assert_eq!(reply[20], 8, "format = stored format");
    }

    #[test]
    fn get_property_full_read_string() {
        let mut dev = dev_with_props();
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(200), AtomId(0), 0, 100, false)
                .unwrap();
        // header 32 + 5 data bytes padded to 8 → 40.
        assert_eq!(reply.len(), 40);
        // length = bytes_to_int32(5) = 2.
        assert_eq!(u32_le(&reply[4..8]), 2);
        assert_eq!(u32_le(&reply[8..12]), XA_STRING.0, "type");
        assert_eq!(u32_le(&reply[12..16]), 0, "bytes_after = 0");
        assert_eq!(u32_le(&reply[16..20]), 5, "num_items = 5");
        assert_eq!(reply[20], 8, "format");
        assert_eq!(&reply[32..37], b"abcde");
        assert_eq!(&reply[37..40], &[0, 0, 0], "value padding");
    }

    #[test]
    fn get_property_full_read_32bit() {
        let mut dev = dev_with_props();
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(300), AtomId(0), 0, 100, false)
                .unwrap();
        assert_eq!(reply.len(), 32 + 8);
        assert_eq!(u32_le(&reply[4..8]), 2, "length = 8 bytes / 4");
        assert_eq!(u32_le(&reply[16..20]), 2, "num_items = 2 (32-bit)");
        assert_eq!(reply[20], 32, "format");
        assert_eq!(u32_le(&reply[32..36]), 0x1122_3344);
        assert_eq!(u32_le(&reply[36..40]), 0x5566_7788);
    }

    #[test]
    fn get_property_windowed_read() {
        // 32-bit prop, 8 bytes total. offset=1 (=> 4 bytes in), len=1 (=> 4 bytes).
        let mut dev = dev_with_props();
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(300), AtomId(0), 1, 1, false)
                .unwrap();
        assert_eq!(reply.len(), 32 + 4);
        assert_eq!(u32_le(&reply[4..8]), 1, "length = 4 bytes / 4");
        assert_eq!(u32_le(&reply[12..16]), 0, "bytes_after = 8-(4+4) = 0");
        assert_eq!(u32_le(&reply[16..20]), 1, "num_items = 1");
        // The second 32-bit word.
        assert_eq!(u32_le(&reply[32..36]), 0x5566_7788);
    }

    #[test]
    fn get_property_windowed_read_reports_bytes_after() {
        // offset=0, len=1 (=> 4 bytes) of an 8-byte prop → bytes_after = 4.
        let mut dev = dev_with_props();
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(300), AtomId(0), 0, 1, false)
                .unwrap();
        assert_eq!(reply.len(), 32 + 4);
        assert_eq!(u32_le(&reply[12..16]), 4, "bytes_after = 8-(0+4)");
        assert_eq!(u32_le(&reply[32..36]), 0x1122_3344, "first word");
    }

    #[test]
    fn get_property_bad_offset_is_value_error() {
        let mut dev = dev_with_props();
        // atom 200 is 5 bytes; offset 2 => 8 bytes in > 5 → BadValue.
        let err = encode_get_property_reply(LE, SEQ, &mut dev, AtomId(200), AtomId(0), 2, 1, false)
            .unwrap_err();
        assert_eq!(err, XiPropError::BadValue);
    }

    #[test]
    fn get_property_delete_removes_when_fully_read() {
        let mut dev = dev_with_props();
        assert!(dev.properties.contains_key(&AtomId(200)));
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(200), AtomId(0), 0, 100, true)
                .unwrap();
        assert_eq!(u32_le(&reply[12..16]), 0, "bytes_after = 0 → delete fires");
        assert!(
            !dev.properties.contains_key(&AtomId(200)),
            "delete must remove the property"
        );
    }

    #[test]
    fn get_property_delete_keeps_when_partial_read() {
        let mut dev = dev_with_props();
        // Partial read of the 8-byte prop leaves bytes_after > 0 → no delete.
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(300), AtomId(0), 0, 1, true)
                .unwrap();
        assert!(u32_le(&reply[12..16]) > 0);
        assert!(
            dev.properties.contains_key(&AtomId(300)),
            "partial read must NOT delete"
        );
    }

    #[test]
    fn get_property_delete_does_not_remove_on_mismatch() {
        let mut dev = dev_with_props();
        // Type mismatch path: bytes_after may be 0 but matched=false.
        let _ = encode_get_property_reply(LE, SEQ, &mut dev, AtomId(100), XA_STRING, 0, 100, true)
            .unwrap();
        assert!(
            dev.properties.contains_key(&AtomId(100)),
            "type-mismatch read must not delete"
        );
    }

    #[test]
    fn change_property_replace_roundtrip() {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        let first = apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            8,
            AtomId(100),
            XA_INTEGER,
            &[7],
        )
        .unwrap();
        assert_eq!(
            first,
            PropWhat::Created,
            "first write on absent property must be Created"
        );
        let reply =
            encode_get_property_reply(LE, SEQ, &mut dev, AtomId(100), AtomId(0), 0, 100, false)
                .unwrap();
        assert_eq!(u32_le(&reply[16..20]), 1, "num_items = 1");
        assert_eq!(reply[32], 7, "value round-trips");

        // Replace again overwrites.
        let second = apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            8,
            AtomId(100),
            XA_INTEGER,
            &[9, 10],
        )
        .unwrap();
        assert_eq!(
            second,
            PropWhat::Modified,
            "second write on existing property must be Modified"
        );
        let prop = dev.properties.get(&AtomId(100)).unwrap();
        assert_eq!(prop.data, vec![9, 10]);
    }

    #[test]
    fn change_property_append_roundtrip() {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            8,
            AtomId(100),
            XA_INTEGER,
            &[1, 2],
        )
        .unwrap();
        let what = apply_change_property(
            &mut dev,
            XI_PROP_MODE_APPEND,
            8,
            AtomId(100),
            XA_INTEGER,
            &[3, 4],
        )
        .unwrap();
        assert_eq!(
            what,
            PropWhat::Modified,
            "append onto existing should be Modified"
        );
        assert_eq!(dev.properties[&AtomId(100)].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn change_property_prepend_roundtrip() {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            8,
            AtomId(100),
            XA_INTEGER,
            &[3, 4],
        )
        .unwrap();
        let what = apply_change_property(
            &mut dev,
            XI_PROP_MODE_PREPEND,
            8,
            AtomId(100),
            XA_INTEGER,
            &[1, 2],
        )
        .unwrap();
        assert_eq!(
            what,
            PropWhat::Modified,
            "prepend onto existing should be Modified"
        );
        assert_eq!(dev.properties[&AtomId(100)].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn change_property_append_format_mismatch_is_badmatch() {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            8,
            AtomId(100),
            XA_INTEGER,
            &[1],
        )
        .unwrap();
        // Append with format 16 onto a format-8 property → BadMatch.
        let err = apply_change_property(
            &mut dev,
            XI_PROP_MODE_APPEND,
            16,
            AtomId(100),
            XA_INTEGER,
            &[0, 0],
        )
        .unwrap_err();
        assert_eq!(err, XiPropError::BadMatch);
    }

    #[test]
    fn change_property_append_type_mismatch_is_badmatch() {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            8,
            AtomId(100),
            XA_INTEGER,
            &[1],
        )
        .unwrap();
        let err = apply_change_property(
            &mut dev,
            XI_PROP_MODE_APPEND,
            8,
            AtomId(100),
            XA_STRING,
            &[2],
        )
        .unwrap_err();
        assert_eq!(err, XiPropError::BadMatch);
    }

    #[test]
    fn change_property_bad_format_is_badvalue() {
        let mut dev = XiDevice::new(DEVICEID_XTEST_POINTER, NAME_XTEST_POINTER);
        let err = apply_change_property(
            &mut dev,
            XI_PROP_MODE_REPLACE,
            7,
            AtomId(100),
            XA_INTEGER,
            &[1],
        )
        .unwrap_err();
        assert_eq!(err, XiPropError::BadValue);
        assert!(dev.properties.is_empty(), "rejected change adds nothing");
    }

    #[test]
    fn delete_property_removes() {
        let mut dev = dev_with_props();
        assert_eq!(
            apply_delete_property(&mut dev, AtomId(100)),
            Some(PropWhat::Deleted),
            "removing an existing property must report Deleted"
        );
        assert!(!dev.properties.contains_key(&AtomId(100)));
        // Absent delete is a no-op, no panic, and returns None so the
        // dispatch arm knows not to emit XI_PropertyEvent.
        assert_eq!(apply_delete_property(&mut dev, AtomId(100)), None);
        assert_eq!(apply_delete_property(&mut dev, AtomId(12345)), None);
    }

    #[test]
    fn xi2_property_event_bytes() {
        // Layout verification against `xXIPropertyEvent` (XI2proto.h:1049-1066).
        // The wire form is fixed-width (32 bytes), so this also doubles
        // as a regression test for the encoder's pad placement.
        let ev = encode_xi2_property_event(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            137,                    // xi_major — yserver's advertised XInputExtension major
            DEVICEID_XTEST_POINTER, // virtual XTEST pointer deviceid
            1000,
            AtomId(0x123),
            PropWhat::Modified,
        );
        assert_eq!(ev.len(), 32);
        assert_eq!(ev[0], 35, "GenericEvent");
        assert_eq!(ev[1], 137, "extension = XI major opcode");
        assert_eq!(u16::from_le_bytes([ev[2], ev[3]]), 7, "sequenceNumber");
        assert_eq!(
            u32::from_le_bytes([ev[4], ev[5], ev[6], ev[7]]),
            0,
            "length = 0 (no payload)"
        );
        assert_eq!(
            u16::from_le_bytes([ev[8], ev[9]]),
            12,
            "evtype = XI_PropertyEvent"
        );
        assert_eq!(
            u16::from_le_bytes([ev[10], ev[11]]),
            DEVICEID_XTEST_POINTER,
            "deviceid = XTEST pointer",
        );
        assert_eq!(
            u32::from_le_bytes([ev[12], ev[13], ev[14], ev[15]]),
            1000,
            "time"
        );
        assert_eq!(
            u32::from_le_bytes([ev[16], ev[17], ev[18], ev[19]]),
            0x123,
            "property atom"
        );
        assert_eq!(ev[20], 2, "what = Modified");
        // Bytes 21-31 are pad0..pad3.
        assert_eq!(&ev[21..32], &[0u8; 11], "padding bytes are zero");
    }

    #[test]
    fn xi2_property_event_bytes_bigendian() {
        // Byte-order parity: the same field layout but every multi-byte
        // CARD encoded big-endian. This guards against accidentally
        // hard-coding LE in the encoder (which would silently break
        // legacy big-endian clients).
        let ev = encode_xi2_property_event(
            ClientByteOrder::BigEndian,
            SequenceNumber(0x0203),
            137,
            0x0405,
            0x0607_0809,
            AtomId(0x0a0b_0c0d),
            PropWhat::Created,
        );
        assert_eq!(ev.len(), 32);
        assert_eq!(ev[0], 35);
        assert_eq!(ev[1], 137);
        assert_eq!(u16::from_be_bytes([ev[2], ev[3]]), 0x0203);
        assert_eq!(u32::from_be_bytes([ev[4], ev[5], ev[6], ev[7]]), 0);
        assert_eq!(u16::from_be_bytes([ev[8], ev[9]]), 12);
        assert_eq!(u16::from_be_bytes([ev[10], ev[11]]), 0x0405);
        assert_eq!(
            u32::from_be_bytes([ev[12], ev[13], ev[14], ev[15]]),
            0x0607_0809
        );
        assert_eq!(
            u32::from_be_bytes([ev[16], ev[17], ev[18], ev[19]]),
            0x0a0b_0c0d
        );
        assert_eq!(ev[20], 1, "what = Created");
    }

    #[test]
    fn xi2_property_event_deleted_what_byte_is_zero() {
        let ev = encode_xi2_property_event(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            137,
            DEVICEID_XTEST_POINTER,
            0,
            AtomId(1),
            PropWhat::Deleted,
        );
        assert_eq!(ev[20], 0, "what = Deleted");
    }

    // ------------------------------------------------------------------
    // XI 1.x DevicePropertyNotify (32-byte wire event)
    // ------------------------------------------------------------------

    #[test]
    fn xi1_device_property_notify_bytes() {
        // Layout verification against `devicePropertyNotify` in
        // /usr/include/X11/extensions/XIproto.h (32 bytes). The wire
        // type byte is `first_event + XI_DevicePropertyNotify(=16)`;
        // `deviceid` is the LAST byte. There is NO window field on the
        // wire (Xlib exposes one, but the protocol struct does not).
        let ev = encode_xi1_device_property_notify(
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            /*first_event=*/ 66,
            /*deviceid=*/ DEVICEID_XTEST_POINTER,
            /*time=*/ 50,
            AtomId(0x77),
            /*deleted=*/ false,
        );
        assert_eq!(ev.len(), 32);
        assert_eq!(
            ev[0],
            66 + 16,
            "type = first_event + XI_DevicePropertyNotify"
        );
        assert_eq!(ev[1], 0, "state = PropertyNewValue (Created/Modified)");
        assert_eq!(u16::from_le_bytes([ev[2], ev[3]]), 3, "sequenceNumber");
        assert_eq!(u32::from_le_bytes([ev[4], ev[5], ev[6], ev[7]]), 50, "time");
        assert_eq!(
            u32::from_le_bytes([ev[8], ev[9], ev[10], ev[11]]),
            0x77,
            "atom"
        );
        // Bytes 12-30 are pad0..pad5 + pad4. Pads must be zero.
        assert_eq!(&ev[12..31], &[0u8; 19], "padding bytes are zero");
        assert_eq!(ev[31], 4, "deviceid = XTEST pointer (last byte)");
    }

    #[test]
    fn xi1_device_property_notify_deleted_state_byte_is_one() {
        // PropertyDelete = 1 (mirror of the XI2 `what` byte test). Same
        // wire-type byte, but `state` flips to 1.
        let ev = encode_xi1_device_property_notify(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            66,
            DEVICEID_XTEST_POINTER,
            0,
            AtomId(1),
            /*deleted=*/ true,
        );
        assert_eq!(ev[1], 1, "state = PropertyDelete");
    }

    #[test]
    fn xi1_device_property_notify_bytes_bigendian() {
        // Byte-order parity: every multi-byte CARD encoded big-endian.
        // Mirrors the matching XI2 BE test above so legacy big-endian
        // clients are protected from any accidental LE hard-coding.
        let ev = encode_xi1_device_property_notify(
            ClientByteOrder::BigEndian,
            SequenceNumber(0x0203),
            66,
            0x7f,
            0x0607_0809,
            AtomId(0x0a0b_0c0d),
            /*deleted=*/ false,
        );
        assert_eq!(ev.len(), 32);
        assert_eq!(ev[0], 66 + 16);
        assert_eq!(ev[1], 0);
        assert_eq!(u16::from_be_bytes([ev[2], ev[3]]), 0x0203);
        assert_eq!(
            u32::from_be_bytes([ev[4], ev[5], ev[6], ev[7]]),
            0x0607_0809
        );
        assert_eq!(
            u32::from_be_bytes([ev[8], ev[9], ev[10], ev[11]]),
            0x0a0b_0c0d
        );
        assert_eq!(ev[31], 0x7f);
    }

    #[test]
    fn get_property_bigendian_header() {
        // Verify the encoder honours BigEndian for the header fields.
        let mut dev = dev_with_props();
        let reply = encode_get_property_reply(
            ClientByteOrder::BigEndian,
            SEQ,
            &mut dev,
            AtomId(100),
            AtomId(0),
            0,
            100,
            false,
        )
        .unwrap();
        // sequence at bytes 2-3, big-endian.
        assert_eq!(u16::from_be_bytes([reply[2], reply[3]]), 0x1234);
        // type atom (INTEGER = 19) big-endian at 8-11.
        assert_eq!(
            u32::from_be_bytes([reply[8], reply[9], reply[10], reply[11]]),
            19
        );
    }

    #[test]
    fn property_atoms_are_stable_across_calls() {
        let mut state = crate::server::ServerState::new();
        let info = touchpad_info(true);
        state.xi_register_source(&info);
        let atom_first = state.atoms.intern(PROP_TAPPING_ENABLED, false);
        state.xi_unregister_source(info.source_id);
        let atom_second = state.atoms.intern(PROP_TAPPING_ENABLED, false);

        // Same atom id across re-seed.
        assert_eq!(atom_first, atom_second);
    }

    // -----------------------------------------------------------------
    // XI device-type atom tests (MOUSE / KEYBOARD / TOUCHPAD)
    // -----------------------------------------------------------------

    /// The three XI device-type atoms are interned at `ServerState`
    /// construction so that `InternAtom(only_if_exists=true)` finds them
    /// without a prior explicit intern — the MATE settings daemon checks
    /// for "TOUCHPAD" this way at session start.
    #[test]
    fn xi_device_type_atoms_interned_at_construction() {
        let mut state = crate::server::ServerState::new();
        // only_if_exists=true returns AtomId(0) for unknown names, so a
        // non-zero return proves the atom was pre-interned.
        let mouse = state.atoms.intern(XI_ATOM_MOUSE, true);
        let keyboard = state.atoms.intern(XI_ATOM_KEYBOARD, true);
        let touchpad = state.atoms.intern(XI_ATOM_TOUCHPAD, true);
        assert_ne!(mouse.0, 0, "MOUSE must be interned at startup");
        assert_ne!(keyboard.0, 0, "KEYBOARD must be interned at startup");
        assert_ne!(touchpad.0, 0, "TOUCHPAD must be interned at startup");
        // All three are distinct atoms.
        assert_ne!(mouse, keyboard, "MOUSE != KEYBOARD");
        assert_ne!(mouse, touchpad, "MOUSE != TOUCHPAD");
        assert_ne!(keyboard, touchpad, "KEYBOARD != TOUCHPAD");
    }

    /// `is_touchpad` is false on all four initial devices.
    #[test]
    fn initial_devices_have_is_touchpad_false() {
        let devs = initial_xi_devices();
        for d in &devs {
            assert!(
                !d.is_touchpad,
                "device {} must have is_touchpad=false at init",
                d.id
            );
        }
    }

    /// Registering and unregistering a source adds and removes its touchpad facet.
    #[test]
    fn seed_and_clear_flip_is_touchpad_flag() {
        let mut state = crate::server::ServerState::new();
        let info = touchpad_info(true);
        let id = state.xi_register_source(&info)[0];
        assert!(state.xi_devices.device(id).unwrap().is_touchpad);
        state.xi_unregister_source(info.source_id);
        assert!(state.xi_devices.device(id).is_none());
    }

    /// `device_is_touchpad` reflects the registry state.
    #[test]
    fn device_is_touchpad_helper_reflects_registry() {
        let mut state = crate::server::ServerState::new();
        let info = touchpad_info(true);
        let id = state.xi_register_source(&info)[0];
        assert!(
            device_is_touchpad(&state.xi_devices, id),
            "true for the physical touchpad facet"
        );
        state.xi_unregister_source(info.source_id);
        assert!(!device_is_touchpad(&state.xi_devices, id));
        // Non-touchpad devices always return false.
        assert!(
            !device_is_touchpad(&state.xi_devices, DEVICEID_MASTER_POINTER),
            "master pointer is never touchpad"
        );
        assert!(
            !device_is_touchpad(&state.xi_devices, DEVICEID_MASTER_KEYBOARD),
            "master keyboard is never touchpad"
        );
        assert!(
            !device_is_touchpad(&state.xi_devices, DEVICEID_XTEST_KEYBOARD),
            "slave keyboard is never touchpad"
        );
    }
}
