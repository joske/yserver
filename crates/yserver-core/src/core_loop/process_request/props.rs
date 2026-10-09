use super::*;

pub(super) fn window_host_xid(state: &ServerState, window: ResourceId) -> u32 {
    state
        .resources
        .window(window)
        .and_then(|w| w.host_xid)
        .map(|h| h.as_raw())
        .unwrap_or(window.0)
}

/// Why the shared property-change dispatch helper rejected a request.
///
/// Each variant maps 1:1 to a wire X error: the caller turns it into an
/// `emit_x11_error_with_minor` call with the variant-specific code and
/// error_value. Kept inside `process_request.rs` so the field types
/// don't leak ServerState into `xinput::libinput_props`. Variants
/// share the `Bad` prefix on purpose — they mirror the X11 error code
/// constants (BadAtom = 5, BadAccess = 10, BadValue = 2, BadMatch = 8).
#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
pub(in crate::core_loop) enum PropertyDispatchError {
    /// Atom is not interned → X error code 5 (BadAtom). Echoes the atom.
    BadAtom { atom: u32 },
    /// Write targets a ReadOnly descriptor → X error code 10 (BadAccess).
    /// Echoes the atom.
    BadAccess { atom: u32 },
    /// Value failed `validate_value` / `decode_change` / backend
    /// rejected it as `Invalid` → X error code 2 (BadValue). Echoes the
    /// wire `format` for spec parity with the other BadValue sites in
    /// the property arms.
    BadValue { error_value: u32 },
    /// Backend rejected the setting as `Unsupported` → X error code 8
    /// (BadMatch). error_value is always 0 here (matches xserver).
    BadMatch,
    /// The requested XI id disappeared or now names a different source.
    BadDevice { deviceid: u16 },
}

/// A validated physical driver write, ready for Task 8's source-targeted
/// backend submission and subsequent commit.
#[derive(Debug, Clone)]
pub(in crate::core_loop) struct ValidatedXiChange {
    pub request: crate::core_loop::message::XiConfigRequest,
    pub source_id: crate::xinput::InputSourceId,
    /// The live XI id for this physical facet.
    pub facet_id: u16,
    pub facet_kind: crate::xinput::XiFacetKind,
    /// Fully merged, normalized value. Flags are retained for an existing
    /// entry and default to the XICreateDeviceProperty flags for recreation.
    pub merged_property: crate::xinput::XiProperty,
    pub change: crate::xinput::libinput_props::DeviceConfigChange,
}

/// XI request headers are normalized by the reader, while the typed value
/// tail remains opaque. Store 16/32-bit property items in the same LE form as
/// the rest of the XI property registry and the libinput decoders.
pub(super) fn canonicalize_xi_property_data(
    byte_order: x11::ClientByteOrder,
    format: u8,
    data: &[u8],
) -> Vec<u8> {
    let mut canonical = data.to_vec();
    if byte_order == x11::ClientByteOrder::BigEndian {
        match format {
            16 => canonical
                .chunks_exact_mut(2)
                .for_each(|value| value.reverse()),
            32 => canonical
                .chunks_exact_mut(4)
                .for_each(|value| value.reverse()),
            _ => {}
        }
    }
    canonical
}

/// Validate and merge one recognized physical libinput property write.
///
/// Support comes from the live source/facet snapshot and descriptor table,
/// never from whether the property happens to be present in the XI map.
/// This lets deleted supported properties be recreated and prevents an
/// ordinary client property from manufacturing driver support.
pub(in crate::core_loop) fn validate_xi_change(
    state: &ServerState,
    request: &crate::core_loop::message::XiConfigRequest,
) -> Result<ValidatedXiChange, PropertyDispatchError> {
    if !state.atoms.exists(request.property) {
        return Err(PropertyDispatchError::BadAtom {
            atom: request.property.0,
        });
    }
    if is_xtest_marker_property(state, request.deviceid, request.property) {
        return Err(PropertyDispatchError::BadAccess {
            atom: request.property.0,
        });
    }

    let device =
        state
            .xi_devices
            .device(request.deviceid)
            .ok_or(PropertyDispatchError::BadDevice {
                deviceid: request.deviceid,
            })?;
    if device.source_id != Some(request.expected_source) {
        return Err(PropertyDispatchError::BadDevice {
            deviceid: request.deviceid,
        });
    }
    let source = state.xi_devices.source(request.expected_source).ok_or(
        PropertyDispatchError::BadDevice {
            deviceid: request.deviceid,
        },
    )?;

    let property_name =
        state
            .atoms
            .name(request.property)
            .ok_or(PropertyDispatchError::BadAtom {
                atom: request.property.0,
            })?;
    let descriptor = crate::xinput::libinput_props::descriptor_by_name(property_name)
        .ok_or(PropertyDispatchError::BadMatch)?;

    // Every recognized libinput descriptor belongs to the pointer facet.
    // A mixed physical source's keyboard facet cannot borrow its sibling's
    // pointer support.
    if device.facet != Some(crate::xinput::XiFacetKind::PointerTouch)
        || !source.capabilities.pointer
    {
        return Err(PropertyDispatchError::BadMatch);
    }

    if descriptor.access == crate::xinput::libinput_props::Access::ReadOnly {
        return Err(PropertyDispatchError::BadAccess {
            atom: request.property.0,
        });
    }

    let expected_type = crate::xinput::type_atom_for(descriptor.val, state.float_atom);
    if request.format != descriptor.format || request.type_atom != expected_type {
        return Err(PropertyDispatchError::BadMatch);
    }

    let existing = device.properties.get(&request.property);
    let merged_data = match (request.mode, existing) {
        (mode, Some(existing)) if mode == crate::xinput::XI_PROP_MODE_APPEND => {
            if existing.format != request.format || existing.type_atom != request.type_atom {
                return Err(PropertyDispatchError::BadMatch);
            }
            let mut data = existing.data.clone();
            data.extend_from_slice(&request.data);
            data
        }
        (mode, Some(existing)) if mode == crate::xinput::XI_PROP_MODE_PREPEND => {
            if existing.format != request.format || existing.type_atom != request.type_atom {
                return Err(PropertyDispatchError::BadMatch);
            }
            let mut data = request.data.clone();
            data.extend_from_slice(&existing.data);
            data
        }
        (mode, None)
            if mode == crate::xinput::XI_PROP_MODE_APPEND
                || mode == crate::xinput::XI_PROP_MODE_PREPEND =>
        {
            request.data.clone()
        }
        _ => request.data.clone(),
    };

    // Match xf86-input-libinput's setter shape checks before disabled-device
    // and availability checks. Format/type mismatches were rejected above.
    // Accel Profile Enabled reports an oversized merged value as BadValue;
    // the other exact-width descriptors retain their Xorg BadMatch result.
    let shape_matches = match descriptor.kind {
        crate::xinput::libinput_props::ValueKind::Scalar => {
            merged_data.len() == usize::from(descriptor.format / 8)
        }
        crate::xinput::libinput_props::ValueKind::OneHot { n }
        | crate::xinput::libinput_props::ValueKind::BitFlags { n } => {
            merged_data.len() == usize::from(n)
        }
        crate::xinput::libinput_props::ValueKind::OneHotOrNone { n, min } => {
            merged_data.len() >= usize::from(min) && merged_data.len() <= usize::from(n)
        }
    };
    if !shape_matches {
        return if descriptor.binding == Some(crate::xinput::libinput_props::Binding::AccelProfile) {
            Err(PropertyDispatchError::BadValue {
                error_value: u32::from(request.format),
            })
        } else {
            Err(PropertyDispatchError::BadMatch)
        };
    }

    if crate::xinput::libinput_props::validate_value(descriptor.kind, request.format, &merged_data)
        .is_err()
    {
        return Err(PropertyDispatchError::BadValue {
            error_value: u32::from(request.format),
        });
    }

    // Scalar bool domains and accel-speed limits are checked by the Xorg
    // setter before it tests whether the physical handle is enabled.
    if matches!(
        descriptor.binding,
        Some(
            crate::xinput::libinput_props::Binding::Tap
                | crate::xinput::libinput_props::Binding::TapDrag
                | crate::xinput::libinput_props::Binding::TapDragLock
                | crate::xinput::libinput_props::Binding::NaturalScroll
                | crate::xinput::libinput_props::Binding::Dwt
                | crate::xinput::libinput_props::Binding::MiddleEmulation
                | crate::xinput::libinput_props::Binding::ScrollButtonLock
        )
    ) && merged_data[0] > 1
    {
        return Err(PropertyDispatchError::BadValue {
            error_value: u32::from(request.format),
        });
    }
    if descriptor.binding == Some(crate::xinput::libinput_props::Binding::AccelSpeed) {
        let speed = f32::from_le_bytes(merged_data[..4].try_into().expect("scalar Float shape"));
        if !(-1.0..=1.0).contains(&speed) && !speed.is_nan() {
            return Err(PropertyDispatchError::BadValue {
                error_value: u32::from(request.format),
            });
        }
    }

    // libinput's property setter sees the source's shared handle, not the
    // target XI facet. The handle stays open while any source facet remains
    // enabled (xf86libinput.c:416-432, 4392-4410).
    if !state
        .xi_devices
        .source_has_enabled_facet(request.expected_source)
    {
        return Err(PropertyDispatchError::BadMatch);
    }
    if !crate::xinput::descriptor_available(descriptor, &source.config) {
        return Err(PropertyDispatchError::BadMatch);
    }

    let binding = descriptor
        .binding
        .expect("writable descriptor must have a config binding");
    let normalized =
        crate::xinput::libinput_props::normalize_value(descriptor.kind, &merged_data).into_owned();
    let change =
        crate::xinput::libinput_props::decode_change(binding, &normalized).map_err(|_| {
            PropertyDispatchError::BadValue {
                error_value: u32::from(request.format),
            }
        })?;

    // A syntactically valid requested profile/mode can still be unavailable
    // on this source. Xorg checks this after its disabled-device gate.
    let requested_value_available = match change {
        crate::xinput::libinput_props::DeviceConfigChange::AccelProfile(Some(profile)) => {
            source.config.accel_profile_available_mask & (1 << profile) != 0
        }
        crate::xinput::libinput_props::DeviceConfigChange::ScrollMethod(Some(method)) => {
            source.config.scroll_method.available_mask & (1 << method) != 0
        }
        crate::xinput::libinput_props::DeviceConfigChange::SendEvents(mask) => {
            mask & !source.config.send_events.available_mask == 0
        }
        _ => true,
    };
    if !requested_value_available {
        return Err(PropertyDispatchError::BadValue {
            error_value: u32::from(request.format),
        });
    }

    let (read_only, deletable) = existing.map_or((false, true), |property| {
        (property.read_only, property.deletable)
    });
    Ok(ValidatedXiChange {
        request: request.clone(),
        source_id: request.expected_source,
        facet_id: request.deviceid,
        facet_kind: device.facet.expect("physical source has a facet"),
        merged_property: crate::xinput::XiProperty {
            type_atom: request.type_atom,
            format: request.format,
            data: normalized,
            read_only,
            deletable,
        },
        change,
    })
}

/// Commit one previously validated property only after its source-targeted
/// backend operation succeeds. A missing property is recreated with
/// XICreateDeviceProperty's writable/deletable defaults.
pub(in crate::core_loop) fn commit_validated_xi_change(
    state: &mut ServerState,
    change: &ValidatedXiChange,
) -> Result<crate::xinput::PropWhat, PropertyDispatchError> {
    if state.xi_devices.source(change.source_id).is_none() {
        return Err(PropertyDispatchError::BadDevice {
            deviceid: change.facet_id,
        });
    }
    let device = state
        .xi_devices
        .device_mut(change.facet_id)
        .filter(|device| {
            device.source_id == Some(change.source_id) && device.facet == Some(change.facet_kind)
        })
        .ok_or(PropertyDispatchError::BadDevice {
            deviceid: change.facet_id,
        })?;

    let (what, read_only, deletable) = match device.properties.get(&change.request.property) {
        Some(property) => (
            crate::xinput::PropWhat::Modified,
            property.read_only,
            property.deletable,
        ),
        None => (crate::xinput::PropWhat::Created, false, true),
    };
    let mut property = change.merged_property.clone();
    property.read_only = read_only;
    property.deletable = deletable;
    device.properties.insert(change.request.property, property);
    Ok(what)
}

/// Commit a confirmed source change against the current XI registry. When a
/// protocol continuation survived in the current generation, preserve its
/// already validated/merged bytes. After disconnect or reset, derive the
/// current atom and property bytes from the submitted setting instead of
/// using retired request metadata.
pub(in crate::core_loop) fn commit_confirmed_xi_change(
    state: &mut ServerState,
    source_id: crate::xinput::InputSourceId,
    change: crate::xinput::libinput_props::DeviceConfigChange,
    validated: Option<&ValidatedXiChange>,
) -> Result<(u16, AtomId, crate::xinput::PropWhat), PropertyDispatchError> {
    if let Some(validated) = validated {
        let what = commit_validated_xi_change(state, validated)?;
        return Ok((validated.facet_id, validated.request.property, what));
    }

    if state.xi_devices.source(source_id).is_none() {
        return Err(PropertyDispatchError::BadDevice { deviceid: 0 });
    }
    let facet_id = state
        .xi_devices
        .facet(source_id, crate::xinput::XiFacetKind::PointerTouch)
        .ok_or(PropertyDispatchError::BadDevice { deviceid: 0 })?;
    let descriptor = crate::xinput::libinput_props::descriptor_for_change(change);
    let property = state.atoms.intern(descriptor.name, false);
    let type_atom = crate::xinput::type_atom_for(descriptor.val, state.float_atom);
    let device = state
        .xi_devices
        .device_mut(facet_id)
        .filter(|device| {
            device.source_id == Some(source_id)
                && device.facet == Some(crate::xinput::XiFacetKind::PointerTouch)
        })
        .ok_or(PropertyDispatchError::BadDevice { deviceid: facet_id })?;
    let (what, read_only, deletable) = match device.properties.get(&property) {
        Some(existing) => (
            crate::xinput::PropWhat::Modified,
            existing.read_only,
            existing.deletable,
        ),
        None => (crate::xinput::PropWhat::Created, false, true),
    };
    device.properties.insert(
        property,
        crate::xinput::XiProperty {
            type_atom,
            format: descriptor.format,
            data: crate::xinput::libinput_props::encode_change_value(change),
            read_only,
            deletable,
        },
    );
    Ok((facet_id, property, what))
}

/// Apply a parsed XI property request. Recognized writes on physical sources
/// use the facet-aware validator; unknown properties and virtual/master
/// properties retain ordinary XI storage behavior.
pub(super) fn is_xtest_marker_property(
    state: &ServerState,
    deviceid: u16,
    property: AtomId,
) -> bool {
    matches!(
        deviceid,
        crate::xinput::DEVICEID_XTEST_POINTER | crate::xinput::DEVICEID_XTEST_KEYBOARD
    ) && property == state.xtest_device_atom
}

pub(in crate::core_loop) enum PropertyChangeOutcome {
    Changed(crate::xinput::PropWhat),
    Pending(crate::core_loop::message::XiConfigRequest),
}

pub(super) fn dispatch_change_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client: ClientId,
    sequence: SequenceNumber,
    minor_opcode: u16,
    deviceid: u16,
    mode: u8,
    format: u8,
    property: AtomId,
    type_atom: AtomId,
    data: &[u8],
) -> Result<PropertyChangeOutcome, PropertyDispatchError> {
    // 1. BadAtom.
    if !state.atoms.exists(property) {
        return Err(PropertyDispatchError::BadAtom { atom: property.0 });
    }
    if is_xtest_marker_property(state, deviceid, property) {
        return Err(PropertyDispatchError::BadAccess { atom: property.0 });
    }
    // 2. Resolve source/facet identity before descriptor lookup. Names and
    //    node paths are metadata only; virtual devices with client-created
    //    libinput-named properties keep ordinary Xorg property behavior.
    let device = crate::xinput::find_device(&state.xi_devices, deviceid)
        .expect("caller verified device exists");
    let prop_name = state.atoms.name(property).map(str::to_owned);
    let source_id = device.source_id;
    let source_info = source_id.and_then(|source| state.xi_devices.source(source));
    if let Some(source_info) = source_info
        && let Some(name) = prop_name.as_deref()
        && crate::xinput::libinput_props::descriptor_by_name(name).is_some()
    {
        let request = crate::core_loop::message::XiConfigRequest {
            client,
            sequence,
            minor_opcode,
            deviceid,
            expected_source: source_info.source_id,
            property,
            type_atom,
            format,
            mode,
            data: data.to_vec(),
        };
        return Ok(PropertyChangeOutcome::Pending(request));
    }

    if source_info.is_some()
        && matches!(
            prop_name.as_deref(),
            Some("Device Node" | "Device Product ID")
        )
    {
        return Err(PropertyDispatchError::BadAccess { atom: property.0 });
    }
    if crate::xinput::find_device(&state.xi_devices, deviceid)
        .and_then(|device| device.properties.get(&property))
        .is_some_and(|property| property.read_only)
    {
        return Err(PropertyDispatchError::BadAccess { atom: property.0 });
    }

    if property == state.xi_device_enabled_atom {
        return dispatch_device_enabled_property_write(
            state, backend, deviceid, mode, format, property, type_atom, data,
        );
    }

    let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid)
        .expect("caller verified device exists");
    match crate::xinput::apply_change_property(device, mode, format, property, type_atom, data) {
        Ok(what) => Ok(PropertyChangeOutcome::Changed(what)),
        Err(crate::xinput::XiPropError::BadValue) => Err(PropertyDispatchError::BadValue {
            error_value: u32::from(format),
        }),
        Err(crate::xinput::XiPropError::BadMatch) => Err(PropertyDispatchError::BadMatch),
        // `apply_change_property` only returns BadValue / BadMatch;
        // BadDevice is handled by the caller's lookup, so no BadDevice
        // arm is reachable here.
        Err(crate::xinput::XiPropError::BadDevice) => {
            unreachable!("apply_change_property never returns BadDevice; lookup handled above")
        }
    }
}

/// Apply one Device Enabled property write. The wire handlers have already
/// validated the device, mode, format range, and request length. Physical
/// facet transitions share the lifecycle emitters with VT changes, while
/// the backend drains that facet's held input before a disable is committed.
fn dispatch_device_enabled_property_write(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    deviceid: u16,
    mode: u8,
    format: u8,
    property: AtomId,
    type_atom: AtomId,
    data: &[u8],
) -> Result<PropertyChangeOutcome, PropertyDispatchError> {
    if !state.atoms.exists(type_atom) {
        return Err(PropertyDispatchError::BadAtom { atom: type_atom.0 });
    }

    let existing = crate::xinput::find_device(&state.xi_devices, deviceid)
        .and_then(|device| device.properties.get(&property))
        .cloned();
    // Xorg treats a missing property's first write as Replace, regardless of
    // the requested append/prepend mode (xiproperty.c:699-706). GetProperty
    // with delete may have removed this normally non-deletable property.
    let effective_mode = if existing.is_none() {
        crate::xinput::XI_PROP_MODE_REPLACE
    } else {
        mode
    };

    // XIChangeDeviceProperty checks append/prepend type and format before it
    // calls DeviceSetProperty (xiproperty.c:714-717).
    if let Some(existing) = &existing
        && effective_mode != crate::xinput::XI_PROP_MODE_REPLACE
        && (existing.format != format || existing.type_atom != type_atom)
    {
        return Err(PropertyDispatchError::BadMatch);
    }
    let resulting_bytes = if effective_mode == crate::xinput::XI_PROP_MODE_REPLACE {
        data.len()
    } else {
        existing.as_ref().map_or(data.len(), |existing| {
            existing.data.len().saturating_add(data.len())
        })
    };
    if format != 8 || type_atom != crate::xinput::XA_INTEGER || resulting_bytes != 1 {
        return Err(PropertyDispatchError::BadValue {
            error_value: property.0,
        });
    }

    // XIChangeDeviceProperty does not call DeviceSetProperty for an empty
    // append/prepend; it leaves the value alone and still sends the normal
    // Modified notification (xiproperty.c:789-800).
    if effective_mode != crate::xinput::XI_PROP_MODE_REPLACE && data.is_empty() {
        return apply_enabled_property_value(
            state,
            deviceid,
            effective_mode,
            format,
            property,
            type_atom,
            data,
        );
    }

    // The base masters and XTEST slaves may be asserted enabled, but clients
    // cannot disable them (dix/devices.c:153-158).
    let value = if effective_mode == crate::xinput::XI_PROP_MODE_REPLACE {
        data[0]
    } else {
        existing
            .as_ref()
            .expect("append/prepend has an existing property")
            .data[0]
    };
    if matches!(
        deviceid,
        crate::xinput::DEVICEID_MASTER_POINTER
            | crate::xinput::DEVICEID_MASTER_KEYBOARD
            | crate::xinput::DEVICEID_XTEST_POINTER
            | crate::xinput::DEVICEID_XTEST_KEYBOARD
    ) {
        if value == 0 {
            return Err(PropertyDispatchError::BadAccess { atom: property.0 });
        }
        return apply_enabled_property_value(
            state,
            deviceid,
            effective_mode,
            format,
            property,
            type_atom,
            data,
        );
    }

    let Some(device) = crate::xinput::find_device(&state.xi_devices, deviceid) else {
        unreachable!("caller resolved the XI device before property dispatch");
    };
    if device.source_id.is_none() || device.facet.is_none() {
        return apply_enabled_property_value(
            state,
            deviceid,
            effective_mode,
            format,
            property,
            type_atom,
            data,
        );
    }

    let was_enabled = device.enabled;
    if value == 0 {
        if was_enabled {
            backend.disable_xi_facet(state, deviceid);
            // Xorg's DeviceSetProperty only calls DisableDevice for an
            // enabled device (devices.c:160-165). A zero written while the
            // session already has the facet off records no client preference.
            state.xi_set_facet_client_disabled(deviceid, true);
            crate::xinput::hotplug::publish_facet_disabled(state, deviceid);
        }
    } else {
        state.xi_set_facet_client_disabled(deviceid, false);
        let is_enabled = crate::xinput::find_device(&state.xi_devices, deviceid)
            .is_some_and(|device| device.enabled);
        if !was_enabled && is_enabled {
            backend.enable_xi_facet(state, deviceid);
            crate::xinput::hotplug::publish_facet_enabled(state, deviceid);
        }
    }

    // XIChangeDeviceProperty stores the caller's complete value and sends its
    // own notification after the handler returns (xiproperty.c:785-801).
    // For a transition this follows the lifecycle sequence above and keeps
    // nonzero values such as 5 intact.
    apply_enabled_property_value(
        state,
        deviceid,
        effective_mode,
        format,
        property,
        type_atom,
        data,
    )
}

fn apply_enabled_property_value(
    state: &mut ServerState,
    deviceid: u16,
    mode: u8,
    format: u8,
    property: AtomId,
    type_atom: AtomId,
    data: &[u8],
) -> Result<PropertyChangeOutcome, PropertyDispatchError> {
    let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid)
        .expect("caller resolved the XI device before property dispatch");
    match crate::xinput::apply_change_property(device, mode, format, property, type_atom, data) {
        Ok(what) => Ok(PropertyChangeOutcome::Changed(what)),
        Err(crate::xinput::XiPropError::BadValue) => Err(PropertyDispatchError::BadValue {
            error_value: u32::from(format),
        }),
        Err(crate::xinput::XiPropError::BadMatch) => Err(PropertyDispatchError::BadMatch),
        Err(crate::xinput::XiPropError::BadDevice) => {
            unreachable!("apply_change_property never returns BadDevice")
        }
    }
}

/// Map a [`PropertyDispatchError`] onto the XI minor-opcode error-emit
/// path. Pure plumbing — the variant carries the error code + value.
pub(in crate::core_loop) fn emit_property_dispatch_error(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    err: PropertyDispatchError,
    minor_opcode: u16,
) -> io::Result<RequestOutcome> {
    let (code, bad_value) = match err {
        PropertyDispatchError::BadAtom { atom } => (5u8, atom),
        PropertyDispatchError::BadAccess { atom } => (10u8, atom),
        PropertyDispatchError::BadValue { error_value } => (2u8, error_value),
        PropertyDispatchError::BadMatch => (8u8, 0u32),
        PropertyDispatchError::BadDevice { deviceid } => (XI2_FIRST_ERROR, u32::from(deviceid)),
    };
    emit_x11_error_with_minor(
        state,
        client_id,
        sequence,
        code,
        bad_value,
        minor_opcode,
        XI2_MAJOR_OPCODE,
    )
}

/// Emit the property-change notification(s) to every client that
/// selected for them on this device. Single fan-out for both XI2
/// (`XI_PropertyEvent`, evtype 12) and XI1 (`DevicePropertyNotify`,
/// XI1 event code 16 within the XInput event block, delivered as wire
/// type `XI_FIRST_EVENT + 16 = 82`), keyed by the same
/// `(deviceid, property, what)` tuple so the two notifications can
/// never diverge.
///
/// Delivery follows xserver's `send_property_event`
/// (`Xi/xiproperty.c:189-208`) via `SendEventToAllWindows`: every
/// client that has selected `XI_PropertyEventMask` (XI2) or a matching
/// `XEventClass` via `SelectExtensionEvent` (XI1) for the affected
/// device on **any** window receives one copy of the event. Because
/// neither `xXIPropertyEvent` nor `devicePropertyNotify` carries an
/// event-window field, per-window distinctions only affect *which
/// clients* receive — the wire bytes are identical for every
/// recipient. Deduplication by client id is handled inside
/// `fanout_event_to_clients`.
///
/// `deviceid` is the literal device id the change applied to (the
/// reserved XTEST ID for a virtual-device change, or a registry-assigned
/// facet ID for a physical source). The XISelectEvents storage keys masks by
/// the deviceid the client *requested*, which may be `XIAllDevices(0)`
/// (a client that wants events from every device) or
/// `XIAllMasterDevices(1)` (every master). xserver's delivery walks
/// `dix/events.c::EventMaskForClient` and OR's the masks stored under
/// the specific id AND the wildcard ids — we must mirror that here, or
/// a client that called `xinput watch-props` (which selects with
/// `deviceid=0`) silently receives nothing. The XI2 and XI1 emits are
/// independent: a client may have selected via either (or both) paths
/// and receives one copy per path.
pub(crate) fn emit_property_change(
    state: &mut ServerState,
    deviceid: u16,
    property: AtomId,
    what: crate::xinput::PropWhat,
) -> Vec<ClientId> {
    // XI2 emit: clients select XI_PropertyEvent via
    // `xi2_masks[(window, deviceid)]` keyed by the deviceid they asked
    // for. A client targeting `XIAllDevices(0)` or — when the event
    // device is a master — `XIAllMasterDevices(1)` selects with that
    // wildcard id, so a property event on a master reaches its exact
    // selector AND `dev == 1` selectors. A slave property event reaches
    // its exact selector AND `dev == 0` selectors. xinput's
    // `watch-props` uses `dev == 0`; without the wildcard match it
    // sees nothing.
    // XI device-id wildcards from `XI2.h`: XIAllDevices=0 selects any
    // device; XIAllMasterDevices=1 selects any master. Physical touchpad
    // facets use registry-assigned IDs; the reserved XTEST ID is only
    // used for virtual-device properties. Slaves never match the
    // master-only wildcard.
    const XI_ALL_DEVICES: u16 = 0;
    let xi2_targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            let selected = client.xi2_masks.iter().any(|(&(_, dev), &mask)| {
                let device_matches = dev == deviceid || dev == XI_ALL_DEVICES;
                device_matches && (mask & u64::from(XI2_PROPERTY_EVENT_MASK)) != 0
            });
            selected.then_some(ClientId(*id))
        })
        .collect();
    let time = state.timestamp_now();
    let mut dropped = Vec::new();
    if !xi2_targets.is_empty() {
        dropped.extend(fanout_event_to_clients(
            state,
            &xi2_targets,
            |buf, seq, order| {
                let event = crate::xinput::encode_xi2_property_event(
                    order,
                    seq,
                    XI2_MAJOR_OPCODE,
                    deviceid,
                    time,
                    property,
                    what,
                );
                buf.extend_from_slice(&event);
            },
        ));
    }
    // XI1 `DevicePropertyNotify` fan-out, keyed off
    // `ClientState::xi1_event_classes`. A client selects via
    // `SelectExtensionEvent` (minor 6) and the recorded class is
    // `(deviceid << 8) | (XI_FIRST_EVENT + 16)` — a property delivery
    // matches only when the client has that exact class. The XI2 and
    // XI1 emits are independent: a client may have selected via either
    // (or both) paths and receives one copy per path.
    //
    // XI1 deviceids are CARD8 on the wire, so the high byte of
    // `xi1_class` is always zero — `as u8` truncations in the
    // SelectExtensionEvent handler (minor 6) rely on this invariant.
    debug_assert!(deviceid <= 0xFF, "XI1 deviceid must fit in CARD8");
    let xi1_class =
        (u32::from(deviceid) << 8) | u32::from(XI_FIRST_EVENT + XI_DEVICE_PROPERTY_NOTIFY_OFFSET);
    let xi1_targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter_map(|(id, client)| {
            client
                .xi1_event_classes
                .contains(&xi1_class)
                .then_some(ClientId(*id))
        })
        .collect();
    if !xi1_targets.is_empty() {
        let deleted = matches!(what, crate::xinput::PropWhat::Deleted);
        dropped.extend(fanout_event_to_clients(
            state,
            &xi1_targets,
            |buf, seq, order| {
                let event = crate::xinput::encode_xi1_device_property_notify(
                    order,
                    seq,
                    XI_FIRST_EVENT,
                    deviceid,
                    time,
                    property,
                    deleted,
                );
                buf.extend_from_slice(&event);
            },
        ));
    }
    dropped
}

pub(super) fn handle_intern_atom(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let name = x11::intern_atom_name(body);
    let atom = state.atoms.intern(&name, header.data != 0);
    debug!(
        "client {} #{} InternAtom {:?} -> {}",
        client_id.0, sequence.0, name, atom.0
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_intern_atom_reply(&mut buf, byte_order, sequence, atom)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_list_properties(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} ListProperties", client_id.0, sequence.0);
    let atoms: Vec<AtomId> = if body.len() >= 4 {
        let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        state
            .resources
            .window(window)
            .map(|w| w.properties.keys().copied().collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32 + atoms.len() * 4);
    x11::write_list_properties_reply(&mut buf, byte_order, sequence, &atoms)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_get_atom_name(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let atom = x11::request_atom(body);
    debug!(
        "client {} #{} GetAtomName {}",
        client_id.0, sequence.0, atom.0
    );
    let local = state.atoms.name(atom).map(str::to_owned);
    let name = match local {
        Some(n) => Some(n),
        None => backend.get_atom_name(origin, atom.0).ok().flatten(),
    };
    match name {
        Some(name) => {
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let byte_order = client.byte_order;
            let mut buf: Vec<u8> = Vec::with_capacity(32 + name.len());
            x11::write_get_atom_name_reply(&mut buf, byte_order, sequence, &name)?;
            Ok(write_to_client(client, client_id, &buf))
        }
        None => emit_x11_error(state, client_id, sequence, x11::error::BAD_ATOM, atom.0, 17),
    }
}

pub(super) fn handle_change_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(req) = x11::change_property_request(header.data, body) else {
        // Diagnostic: dump the request header + body shape that we
        // rejected as malformed, so we can see what the client sent.
        let preview_len = body.len().min(64);
        let body_preview = &body[..preview_len];
        debug!(
            "ChangeProperty BadLength (parse-fail): client={} seq={} \
             header.data={:#x} body.len={} length_units={} preview={:02x?}",
            client_id.0,
            sequence.0,
            header.data,
            body.len(),
            header.length_units,
            body_preview,
        );
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, 18);
    };
    let Some(mode) = properties::ChangeMode::from_protocol(req.mode) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.mode),
            18,
        );
    };
    let Some(format) = properties::PropertyFormat::from_protocol(req.format) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.format),
            18,
        );
    };
    let expected_bytes = (req.length as usize).checked_mul(format.bytes());
    if expected_bytes != Some(req.data.len()) {
        debug!(
            "ChangeProperty BadLength (length mismatch): client={} seq={} \
             req.length={} format={} expected={:?} got={}",
            client_id.0,
            sequence.0,
            req.length,
            req.format,
            expected_bytes,
            req.data.len(),
        );
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, 18);
    }
    if state.resources.window(req.window).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            req.window.0,
            18,
        );
    }
    if !state.atoms.exists(req.property) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.property.0,
            18,
        );
    }
    if !state.atoms.exists(req.r#type) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.r#type.0,
            18,
        );
    }
    // Diagnostic: log WM_CLASS (and _NET_WM_PID) so we can map
    // yserver client_id → user-space process when debugging
    // missing-click / dead-applet issues. WM_CLASS is two
    // null-terminated strings (instance, class); _NET_WM_PID is a
    // CARD32. Both are set once near client init, so noise is
    // bounded.
    if let Some(prop_name) = state.atoms.name(req.property) {
        if prop_name == "WM_CLASS" && req.format == 8 {
            let s = String::from_utf8_lossy(&req.data).replace('\0', " | ");
            let trimmed = s.trim_end_matches(" | ").to_string();
            log::debug!(
                "client {} WM_CLASS on 0x{:x}: {}",
                client_id.0,
                req.window.0,
                trimmed,
            );
            // Mirror into the side-table so the MIT-SHM PutImage perf
            // log (and any future client-attributed diagnostics) can
            // resolve drawable owner → recognisable process name
            // without re-grepping the log.
            state.client_wm_class.entry(client_id.0).or_insert(trimmed);
        } else if prop_name == "_NET_WM_PID" && req.format == 32 && req.data.len() >= 4 {
            let pid = u32::from_le_bytes([req.data[0], req.data[1], req.data[2], req.data[3]]);
            log::debug!(
                "client {} _NET_WM_PID on 0x{:x}: {}",
                client_id.0,
                req.window.0,
                pid,
            );
        } else if prop_name == "_NET_FRAME_EXTENTS" && req.format == 32 && req.data.len() >= 16 {
            // 4× CARDINAL: left, right, top, bottom (frame inset
            // around the client window). GTK reads this to position
            // popups; if it's corrupted we'd see the popup-placement
            // bug for reparented application windows.
            let l = u32::from_le_bytes([req.data[0], req.data[1], req.data[2], req.data[3]]);
            let r = u32::from_le_bytes([req.data[4], req.data[5], req.data[6], req.data[7]]);
            let t = u32::from_le_bytes([req.data[8], req.data[9], req.data[10], req.data[11]]);
            let b = u32::from_le_bytes([req.data[12], req.data[13], req.data[14], req.data[15]]);
            log::info!(
                "client {} _NET_FRAME_EXTENTS on 0x{:x}: left={} right={} top={} bottom={}",
                client_id.0,
                req.window.0,
                l,
                r,
                t,
                b,
            );
        } else if prop_name == "_NET_WORKAREA" && req.format == 32 && req.data.len() >= 16 {
            // 4× CARDINAL per desktop: x, y, width, height. Log
            // the first desktop's quad; subsequent desktops are
            // rare and the first is the one popup placement uses.
            let x = u32::from_le_bytes([req.data[0], req.data[1], req.data[2], req.data[3]]);
            let y = u32::from_le_bytes([req.data[4], req.data[5], req.data[6], req.data[7]]);
            let w = u32::from_le_bytes([req.data[8], req.data[9], req.data[10], req.data[11]]);
            let h = u32::from_le_bytes([req.data[12], req.data[13], req.data[14], req.data[15]]);
            log::info!(
                "client {} _NET_WORKAREA on 0x{:x}: x={} y={} w={} h={} (desktop 0)",
                client_id.0,
                req.window.0,
                x,
                y,
                w,
                h,
            );
        }
    }
    let existing = state
        .resources
        .window_property(req.window, req.property)
        .cloned();
    let new_value =
        match properties::apply_change(existing.as_ref(), mode, req.r#type, format, &req.data) {
            Ok(v) => v,
            Err(properties::ChangePropertyError::BadMatch) => {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    req.window.0,
                    18,
                );
            }
            Err(properties::ChangePropertyError::BadAlloc) => {
                return emit_x11_error(state, client_id, sequence, x11::error::BAD_ALLOC, 0, 18);
            }
            Err(properties::ChangePropertyError::BadValue) => {
                return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, 0, 18);
            }
        };
    state
        .resources
        .set_window_property(req.window, req.property, new_value);
    backend.on_window_property_changed(state, window_host_xid(state, req.window), req.property);
    if req.window == crate::resources::ROOT_WINDOW
        && state.atoms.name(req.property) == Some("_XKB_RULES_NAMES")
        && let Some(value) = state
            .resources
            .window_property(req.window, req.property)
            .map(|p| p.data.clone())
    {
        crate::core_loop::xkb_layout::apply_rules_names_change(state, backend, &value);
    }
    let timestamp = state.timestamp_now();
    let window = req.window;
    let property = req.property;
    let _dropped = emit_window_event_to_state(
        state,
        window,
        0x0040_0000, // PropertyChangeMask
        |buf, seq, order| {
            x11::encode_property_notify_event(buf, seq, order, window, property, timestamp, false);
        },
    );
    debug!("client {} #{} ChangeProperty", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_get_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::ClientByteOrder;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let Some(req) = x11::get_property_request(header.data, body) else {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, 20);
    };
    if state.resources.window(req.window).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            req.window.0,
            20,
        );
    }
    if !state.atoms.exists(req.property) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.property.0,
            20,
        );
    }
    if req.r#type.0 != 0 && !state.atoms.exists(req.r#type) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.r#type.0,
            20,
        );
    }
    // Build the reply against an immutable borrow of state.resources,
    // copy the bytes into an owned buffer, then drop the borrow before
    // we touch state again for the optional delete + fanout.
    let (reply_bytes, type_matched, slice_bytes_after, existing_some) = {
        let existing = state
            .resources
            .window_property(req.window, req.property)
            .cloned();
        let slice = match properties::slice_for_get(
            existing.as_ref(),
            req.r#type,
            req.long_offset,
            req.long_length,
        ) {
            Ok(s) => s,
            Err(properties::ChangePropertyError::BadValue) => {
                return emit_x11_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.long_offset,
                    20,
                );
            }
            Err(_) => unreachable!("slice_for_get only returns BadValue on error"),
        };
        let value_len_units = if slice.format == 0 {
            0
        } else {
            slice.value.len() as u32 / u32::from(slice.format / 8)
        };
        let mut buf: Vec<u8> = Vec::with_capacity(32 + slice.value.len());
        x11::write_get_property_reply(
            &mut buf,
            byte_order,
            sequence,
            x11::GetPropertyReply {
                format: slice.format,
                r#type: slice.r#type,
                bytes_after: slice.bytes_after,
                value_len: value_len_units,
                value: slice.value,
            },
        )?;
        let type_matched = existing
            .as_ref()
            .is_some_and(|p| req.r#type.0 == 0 || req.r#type == p.r#type);
        (buf, type_matched, slice.bytes_after, existing.is_some())
    };
    // Send reply bytes through client_io::write_or_buffer.
    let outcome = match state.clients.get_mut(&client_id.0) {
        Some(client) => write_to_client(client, client_id, &reply_bytes),
        None => RequestOutcome::Handled,
    };
    if matches!(outcome, RequestOutcome::Disconnect(_)) {
        return Ok(outcome);
    }
    // Optional delete + PropertyNotify fanout (state=true) when
    // delete=1 fires per spec.
    if req.delete && type_matched && slice_bytes_after == 0 && existing_some {
        let _ = state
            .resources
            .delete_window_property(req.window, req.property);
        backend.on_window_property_changed(state, window_host_xid(state, req.window), req.property);
        let timestamp = state.timestamp_now();
        let window = req.window;
        let property = req.property;
        let _dropped = emit_window_event_to_state(state, window, 0x0040_0000, |buf, seq, order| {
            x11::encode_property_notify_event(buf, seq, order, window, property, timestamp, true);
        });
    }
    debug!("client {} #{} GetProperty", client_id.0, sequence.0);
    Ok(outcome)
}

pub(super) fn handle_delete_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(req) = x11::delete_property_request(body) else {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, 19);
    };
    if state.resources.window(req.window).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            req.window.0,
            19,
        );
    }
    let removed = state
        .resources
        .delete_window_property(req.window, req.property);
    if removed.is_some() {
        backend.on_window_property_changed(state, window_host_xid(state, req.window), req.property);
        let timestamp = state.timestamp_now();
        let window = req.window;
        let property = req.property;
        let _dropped = emit_window_event_to_state(state, window, 0x0040_0000, |buf, seq, order| {
            x11::encode_property_notify_event(buf, seq, order, window, property, timestamp, true);
        });
    }
    debug!("client {} #{} DeleteProperty", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}
