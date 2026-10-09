use super::*;

pub(in crate::core_loop::process_request) fn handle_xi2_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
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
    let minor = header.data;
    // Per-minor length validation mirroring Xorg's `REQUEST_SIZE_MATCH` /
    // `REQUEST_AT_LEAST_SIZE` macros invoked at the top of each `ProcX*`
    // handler in `xserver/Xi/*.c`. xts5 XIproto probes under-/over-length
    // headers for every XI minor and expects core `BadLength` (code 16),
    // not the extension-level `BadDevice` the handlers would otherwise
    // produce after mis-parsing the truncated/extended body. The XI
    // `minor_opcode` is also stamped into the error reply so the test
    // harness can attribute the failure.
    if !x11::request_lengths::validate_xi_request_length(minor, header.length_units) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    // Content-derived exact-length check for variable-length minors.
    // xts5 probes both `length-1` and `length+1`; the AtLeast gate
    // catches the under-length case while this gate catches the
    // over-length one.
    if !x11::request_lengths::validate_xi_exact_request_length(minor, header.length_units, body) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    // Xorg's XI swap/dispatch path checks these content-derived tails before
    // handling any selection or grab records. The request swapper has already
    // converted the dynamic length fields to LE, while mask bytes remain
    // opaque. Keep this check ahead of all state changes so a truncated later
    // record cannot leave earlier selections or grabs installed.
    if !xi2_request_has_complete_dynamic_tail(minor, body) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    match minor {
        1 => {
            // XI GetExtensionVersion, as Xorg's ProcXGetExtensionVersion
            // (Xi/getvers.c): the length must match `nbytes`, and the reply
            // is the server's XI version (XIVersion, 2.4) with RepType =
            // X_GetExtensionVersion. libXi reads this version to decide
            // whether XI 2.2 fields (a raw event's sourceid) are valid.
            debug!(
                "client {} #{} XIGetExtensionVersion",
                client_id.0, sequence.0
            );
            let nbytes = body
                .get(0..2)
                .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])));
            if usize::try_from(header.length_units).ok() != Some((8 + nbytes).div_ceil(4)) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    1,
                    XI2_MAJOR_OPCODE,
                );
            }
            let mut reply = x11::fixed_reply(byte_order, sequence, 1, 0);
            x11::write_u16(byte_order, &mut reply, XI2_SERVER_MAJOR_VERSION);
            x11::write_u16(byte_order, &mut reply, XI2_SERVER_MINOR_VERSION);
            reply.push(1);
            reply.extend_from_slice(&[0; 19]);
            buf.extend_from_slice(&reply);
        }
        42 => {
            // XIChangeCursor (XI2 opcode 42). Wire body after the
            // request header: window(4), cursor(4), deviceid(2), pad(2).
            // GTK4 (gnome-text-editor, modern GTK apps) sets per-widget
            // cursors via this rather than core `XDefineCursor`, so
            // pre-fix the I-beam never reached the backend and text
            // areas kept showing the default arrow. Treat it as
            // `XDefineCursor` for now — yserver doesn't yet route
            // per-device cursors, but a single effective cursor on the
            // window matches what XInput2 produces for AllMasterDevices
            // and is what GTK relies on in practice. `cursor = None`
            // (xid 0) means "clear" — same semantics as the CWA cursor
            // path, see Xorg `dix/window.c:1487-1491`.
            if body.len() < 8 {
                debug!(
                    "client {} #{} XIChangeCursor (body too short: {})",
                    client_id.0,
                    sequence.0,
                    body.len()
                );
                return Ok(RequestOutcome::Handled);
            }
            let window = ResourceId(u32::from_le_bytes(body[0..4].try_into().expect("4 bytes")));
            let cursor = ResourceId(u32::from_le_bytes(body[4..8].try_into().expect("4 bytes")));
            let host_window_raw = if window == ROOT_WINDOW {
                Some(backend.window_id())
            } else {
                state
                    .resources
                    .window(window)
                    .and_then(|w| w.host_xid)
                    .map(|h| h.as_raw())
            };
            let cursor_host_xid = if cursor.0 == 0 {
                Some(0u32)
            } else {
                state.resources.cursor_host_xid(cursor)
            };
            debug!(
                "client {} #{} XIChangeCursor window=0x{:x} cursor=0x{:x}",
                client_id.0, sequence.0, window.0, cursor.0
            );
            if let (Some(hw), Some(ch)) = (host_window_raw, cursor_host_xid) {
                let _ = backend.define_cursor(origin, hw, ch);
            }
            return Ok(RequestOutcome::Handled);
        }
        44 => {
            debug!("client {} #{} XISetClientPointer", client_id.0, sequence.0);
            return Ok(RequestOutcome::Handled);
        }
        45 => {
            // XIGetClientPointer reply layout (xXIGetClientPointerReply,
            // X11/extensions/XI2proto.h):
            //   response_type(1) | pad0(1) | sequence(2) | length(4)
            //   | set:BOOL(1) | pad0(1) | deviceid(2) | pad1..5(20)
            // Total = 32 bytes. NOTE: deviceid sits at byte 10 — only
            // ONE pad byte follows `set`. Writing 3 pad bytes here pushed
            // deviceid to byte 12, so clients read 0 (broke nemo's
            // desktop rubber-band, which looks up its pointer device via
            // this reply).
            //
            // Must reply `set=True`: libXi's `XIGetClientPointer`
            // only writes the caller's `*deviceid` out-parameter
            // when `reply.set == True`. With `set=False` it returns
            // False and leaves the caller's variable uninitialized,
            // which GDK then passes verbatim to `g_hash_table_lookup`
            // on its device id_table — looking up garbage returns
            // NULL, so GDK assigns `device=NULL` to any synthetic
            // core ButtonPress/ButtonRelease/MotionNotify from
            // XSendEvent (gdkdevicemanager-xi2.c::translate_event
            // calls `get_client_pointer` to attribute the device),
            // and the next `proxy_button_event` SEGVs at
            // `pointer_info->toplevel_under_pointer`. mate-panel
            // dies on every workspace-switch click because wnck-applet
            // SendEvents a synthetic ButtonRelease into the panel
            // when activating a workspace.
            debug!(
                "client {} #{} XIGetClientPointer -> set=1 deviceid=2",
                client_id.0, sequence.0
            );
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            reply.push(1); // byte 8: set = True
            reply.push(0); // byte 9: pad0
            // bytes 10-11: deviceid = master pointer (2).
            x11::write_u16(byte_order, &mut reply, 2);
            reply.extend_from_slice(&[0u8; 20]); // bytes 12-31: pad1..5
            debug_assert_eq!(reply.len(), 32);
            buf.extend_from_slice(&reply);
        }
        46 => {
            debug!("client {} #{} XISelectEvents", client_id.0, sequence.0);
            let mut send_device_changed_bootstrap = false;
            if body.len() >= 8 {
                let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                let num_masks = u16::from_le_bytes([body[4], body[5]]) as usize;
                // Xorg ProcXISelectEvents checks every mask before
                // setting any, so a rejected request changes nothing.
                let mut masks: Vec<(u16, u64)> = Vec::with_capacity(num_masks);
                let mut pos = 8;
                for _ in 0..num_masks {
                    if pos + 4 > body.len() {
                        break;
                    }
                    let deviceid = u16::from_le_bytes([body[pos], body[pos + 1]]);
                    let mask_len = u16::from_le_bytes([body[pos + 2], body[pos + 3]]) as usize;
                    pos += 4;
                    let byte_len = mask_len.saturating_mul(4);
                    if pos + byte_len > body.len() {
                        break;
                    }
                    let mask_bytes = &body[pos..pos + byte_len];
                    // XICheckInvalidMaskBits: a bit past XI2LASTEVENT is
                    // BadValue with the bit number as errorValue.
                    if let Some(bit) = xi2_first_invalid_mask_bit(mask_bytes) {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            bit,
                            46,
                            header.opcode,
                        );
                    }
                    let hierarchy_event = crate::xinput::XI2_HIERARCHY_CHANGED_EVENT_TYPE;
                    let selects_hierarchy = mask_bytes
                        .get((hierarchy_event / 8) as usize)
                        .is_some_and(|byte| byte & (1 << (hierarchy_event % 8)) != 0);
                    if deviceid != 0 && selects_hierarchy {
                        // Xorg Xi/xiselectev.c:186-193 permits this mask only
                        // on XIAllDevices. Preserve its lookup-before-mask
                        // validation order for concrete device ids.
                        if deviceid != 1 && state.xi_devices.device(deviceid).is_none() {
                            return emit_x11_error_with_minor(
                                state,
                                client_id,
                                sequence,
                                XI2_FIRST_ERROR, // XI_BadDevice
                                u32::from(deviceid),
                                46,
                                header.opcode,
                            );
                        }
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            hierarchy_event,
                            46,
                            header.opcode,
                        );
                    }
                    // The mask is a bit array (bit n in byte n/8), so the
                    // bytes are little-endian whatever the client order.
                    let mut word = [0u8; 8];
                    let keep = byte_len.min(8);
                    word[..keep].copy_from_slice(&mask_bytes[..keep]);
                    masks.push((deviceid, u64::from_le_bytes(word)));
                    pos += byte_len;
                }
                if let Some(client) = state.clients.get_mut(&client_id.0) {
                    for (deviceid, mask) in masks {
                        debug!(
                            "client {} XISelectEvents window=0x{:x} deviceid={} mask=0x{:x}",
                            client_id.0, window.0, deviceid, mask
                        );
                        if mask == 0 {
                            client.xi2_masks.remove(&(window, deviceid));
                        } else {
                            client.xi2_masks.insert((window, deviceid), mask);
                            if window == ROOT_WINDOW
                                && matches!(deviceid, 0..=3)
                                && (mask & u64::from(XI2_DEVICE_CHANGED_MASK)) != 0
                            {
                                send_device_changed_bootstrap = true;
                            }
                        }
                    }
                }
            }
            if send_device_changed_bootstrap {
                emit_xi2_device_changed_bootstrap(
                    state,
                    backend,
                    origin,
                    client_id,
                    sequence,
                    XI2_MAJOR_OPCODE,
                )?;
            }
            return Ok(RequestOutcome::Handled);
        }
        47 => {
            debug!("client {} #{} XIQueryVersion", client_id.0, sequence.0);
            let requested_major = body
                .get(0..2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .unwrap_or(0);
            let requested_minor = body
                .get(2..4)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .unwrap_or(0);
            if requested_major < 2 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(requested_major),
                    47,
                    XI2_MAJOR_OPCODE,
                );
            }
            let (reply_major, reply_minor) = if (requested_major, requested_minor)
                < (XI2_SERVER_MAJOR_VERSION, XI2_SERVER_MINOR_VERSION)
            {
                (requested_major, requested_minor)
            } else {
                (XI2_SERVER_MAJOR_VERSION, XI2_SERVER_MINOR_VERSION)
            };
            record_xi2_client_version(state, client_id, (reply_major, reply_minor));
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            x11::write_u16(byte_order, &mut reply, reply_major);
            x11::write_u16(byte_order, &mut reply, reply_minor);
            reply.extend_from_slice(&[0; 20]);
            buf.extend_from_slice(&reply);
        }
        48 => {
            // XIQueryDevice: 0 means all live devices, 1 means the master
            // pair, and every other ID selects one exact registry entry.
            debug!("client {} #{} XIQueryDevice", client_id.0, sequence.0);
            if body.len() < 2 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    48,
                    XI2_MAJOR_OPCODE,
                );
            }
            let request_device_id = match byte_order {
                ClientByteOrder::LittleEndian => u16::from_le_bytes([body[0], body[1]]),
                ClientByteOrder::BigEndian => u16::from_be_bytes([body[0], body[1]]),
            };
            let devices = match state.xi_devices.query(request_device_id) {
                Ok(devices) => devices,
                Err(crate::xinput::XiQueryError::BadDevice(device_id)) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR, // XI_BadDevice
                        u32::from(device_id),
                        48,
                        XI2_MAJOR_OPCODE,
                    );
                }
            };
            let class_data = crate::xinput::query::XiQueryClassData {
                button_labels: [
                    state.atoms.intern("Button Left", false),
                    state.atoms.intern("Button Middle", false),
                    state.atoms.intern("Button Right", false),
                    state.atoms.intern("Button Wheel Up", false),
                    state.atoms.intern("Button Wheel Down", false),
                    state.atoms.intern("Button Horiz Wheel Left", false),
                    state.atoms.intern("Button Horiz Wheel Right", false),
                ],
                button_state: state.buttons_down,
                axis_labels: [
                    state.atoms.intern("Rel X", false),
                    state.atoms.intern("Rel Y", false),
                    state.atoms.intern("Rel Vert Scroll", false),
                    state.atoms.intern("Rel Horiz Scroll", false),
                ],
                pointer: (
                    i32::from(state.randr.screen_width) / 2,
                    i32::from(state.randr.screen_height) / 2,
                ),
                // Xorg ListValuatorInfo reports the master pointer's
                // current axisVal (Xi/xiquerydevice.c:369). This state is
                // synchronized from the latest attached source in pointer
                // fanout, rather than globally accumulating independent
                // physical-device scroll counters.
                scroll: state.scroll_axis_value,
            };
            let reply =
                crate::xinput::query::encode_reply(byte_order, sequence, &devices, class_data)?;
            buf.extend_from_slice(&reply);
        }
        56 => {
            // XIListProperties (xXIListPropertiesReq, XI2proto.h:742-749).
            // body[0..2] = deviceid, body[2..4] = pad.
            if body.len() < 2 {
                return Ok(RequestOutcome::Handled);
            }
            let deviceid = u16::from_le_bytes([body[0], body[1]]);
            debug!(
                "client {} #{} XIListProperties deviceid={}",
                client_id.0, sequence.0, deviceid
            );
            let Some(device) = crate::xinput::find_device(&state.xi_devices, deviceid) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    56,
                    XI2_MAJOR_OPCODE,
                );
            };
            buf.extend_from_slice(&crate::xinput::encode_list_properties_reply(
                byte_order, sequence, device,
            ));
        }
        57 => {
            // XIChangeProperty (xXIChangePropertyReq, XI2proto.h:769-780).
            // Header struct: deviceid(2), mode(1), format(1), property(4),
            // type(4), num_items(4); after stripping the 4-byte generic
            // header `body` begins at deviceid:
            //   body[0..2] deviceid, body[2] mode, body[3] format,
            //   body[4..8] property, body[8..12] type,
            //   body[12..16] num_items, body[16..] value bytes.
            if body.len() < 16 {
                return Ok(RequestOutcome::Handled);
            }
            let deviceid = u16::from_le_bytes([body[0], body[1]]);
            let mode = body[2];
            let format = body[3];
            let property = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let type_atom = AtomId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
            let num_items = u32::from_le_bytes([body[12], body[13], body[14], body[15]]) as usize;
            debug!(
                "client {} #{} XIChangeProperty deviceid={} mode={} format={} property={} type={} num_items={}",
                client_id.0, sequence.0, deviceid, mode, format, property.0, type_atom.0, num_items
            );
            // Device lookup FIRST (xserver ProcXIChangeProperty,
            // xiproperty.c:1137-1141, does dixLookupDevice before the
            // format/mode checks), so an unknown device yields BadDevice
            // even when the format is also invalid.
            if crate::xinput::find_device(&state.xi_devices, deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    57,
                    XI2_MAJOR_OPCODE,
                );
            }
            // Validate mode (xiproperty.c check_change_property:325-329).
            if mode != crate::xinput::XI_PROP_MODE_REPLACE
                && mode != crate::xinput::XI_PROP_MODE_PREPEND
                && mode != crate::xinput::XI_PROP_MODE_APPEND
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    2, // BadValue
                    u32::from(mode),
                    57,
                    XI2_MAJOR_OPCODE,
                );
            }
            // Format must be 8/16/32 before we compute the value length.
            if format != 8 && format != 16 && format != 32 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    2, // BadValue
                    u32::from(format),
                    57,
                    XI2_MAJOR_OPCODE,
                );
            }
            let value_len = num_items.saturating_mul(usize::from(format / 8));
            let Some(data) = body.get(16..16 + value_len) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    16, // BadLength
                    0,
                    57,
                    XI2_MAJOR_OPCODE,
                );
            };
            let data = canonicalize_xi_property_data(byte_order, format, data);
            // T3 spec §D order is implemented by `dispatch_change_property`;
            // both write arms (XI2 minor 57 + XI1 minor 37) share the same
            // pipeline so T5/T6 only has to wire event emission in one place.
            match dispatch_change_property(
                state, backend, client_id, sequence, 57, deviceid, mode, format, property,
                type_atom, &data,
            ) {
                Ok(PropertyChangeOutcome::Changed(what)) => {
                    let _ = emit_property_change(state, deviceid, property, what);
                    return Ok(RequestOutcome::Handled);
                }
                Ok(PropertyChangeOutcome::Pending(request)) => {
                    return Ok(RequestOutcome::PendingXiConfig(request));
                }
                Err(err) => {
                    return emit_property_dispatch_error(state, client_id, sequence, err, 57);
                }
            }
        }
        58 => {
            // XIDeleteProperty (xXIDeletePropertyReq, XI2proto.h:785-793).
            //   body[0..2] deviceid, body[2..4] pad0, body[4..8] property.
            if body.len() < 8 {
                return Ok(RequestOutcome::Handled);
            }
            let deviceid = u16::from_le_bytes([body[0], body[1]]);
            let property = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            debug!(
                "client {} #{} XIDeleteProperty deviceid={} property={}",
                client_id.0, sequence.0, deviceid, property.0
            );
            // T3: BadAtom guard (xserver ProcXIDeleteProperty,
            // xiproperty.c).
            if !state.atoms.exists(property) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    5, // BadAtom
                    property.0,
                    58,
                    XI2_MAJOR_OPCODE,
                );
            }
            if crate::xinput::find_device(&state.xi_devices, deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    58,
                    XI2_MAJOR_OPCODE,
                );
            }
            if is_xtest_marker_property(state, deviceid, property) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    10, // BadAccess: XTEST Device is protected by atom identity.
                    property.0,
                    58,
                    XI2_MAJOR_OPCODE,
                );
            }
            if crate::xinput::find_device(&state.xi_devices, deviceid)
                .and_then(|device| device.properties.get(&property))
                .is_some_and(|property| !property.deletable)
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    10, // BadAccess: seeded driver properties are non-deletable.
                    property.0,
                    58,
                    XI2_MAJOR_OPCODE,
                );
            }
            let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid)
                .expect("device existence verified above");
            if let Some(what) = crate::xinput::apply_delete_property(device, property) {
                // Notify on a real removal only; a redundant delete of
                // an absent property must stay silent — xiproperty.c
                // does not call `send_property_event` in that case.
                let _ = emit_property_change(state, deviceid, property, what);
            }
            return Ok(RequestOutcome::Handled);
        }
        59 => {
            // XIGetProperty (xXIGetPropertyReq, XI2proto.h:798-814).
            // Struct: deviceid(2), delete(1), pad0(1), property(4),
            // type(4), offset(4), len(4). After the 4-byte generic header
            // `body` begins at deviceid:
            //   body[0..2] deviceid, body[2] delete, body[3] pad0,
            //   body[4..8] property, body[8..12] type,
            //   body[12..16] offset, body[16..20] len.
            if body.len() < 20 {
                return Ok(RequestOutcome::Handled);
            }
            let deviceid = u16::from_le_bytes([body[0], body[1]]);
            let delete_raw = body[2];
            let property = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let req_type = AtomId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
            let offset = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
            let len = u32::from_le_bytes([body[16], body[17], body[18], body[19]]);
            debug!(
                "client {} #{} XIGetProperty deviceid={} property={} type={} offset={} len={} delete={}",
                client_id.0, sequence.0, deviceid, property.0, req_type.0, offset, len, delete_raw
            );
            // T3: BadAtom guard before the device lookup so a bogus atom
            // surfaces as BadAtom (xserver ProcXIGetProperty does it the
            // same way via ValidAtom() before dixLookupDevice's payload
            // path; the relative precedence vs. BadDevice never matters
            // because clients always pair a valid atom with the deviceid
            // they just learned from XIListProperties).
            if !state.atoms.exists(property) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    5, // BadAtom
                    property.0,
                    59,
                    XI2_MAJOR_OPCODE,
                );
            }
            // Device lookup first (xserver dixLookupDevice precedes
            // get_property, so BadDevice outranks the BadValue below).
            if crate::xinput::find_device(&state.xi_devices, deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    59,
                    XI2_MAJOR_OPCODE,
                );
            }
            // `delete` must be exactly 0 or 1; xserver's get_property
            // (xiproperty.c:252-255) rejects anything else with BadValue.
            if delete_raw > 1 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    2, // BadValue
                    u32::from(delete_raw),
                    59,
                    XI2_MAJOR_OPCODE,
                );
            }
            let delete = delete_raw != 0;
            // Re-acquire mutably now that both validations passed.
            let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid)
                .expect("device existence verified above");
            let send_deleted = match crate::xinput::encode_get_property_reply(
                byte_order, sequence, device, property, req_type, offset, len, delete,
            ) {
                Ok(reply) => {
                    let returned = x11::read_u32(byte_order, &reply[4..8]);
                    let bytes_after = x11::read_u32(byte_order, &reply[12..16]);
                    buf.extend_from_slice(&reply);
                    delete && returned != 0 && bytes_after == 0
                }
                Err(crate::xinput::XiPropError::BadValue) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        2, // BadValue
                        offset,
                        59,
                        XI2_MAJOR_OPCODE,
                    );
                }
                Err(_) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        2,
                        offset,
                        59,
                        XI2_MAJOR_OPCODE,
                    );
                }
            };
            if send_deleted {
                let _ = emit_property_change(
                    state,
                    deviceid,
                    property,
                    crate::xinput::PropWhat::Deleted,
                );
            }
        }
        60 => {
            debug!("client {} #{} XIGetSelectedEvents", client_id.0, sequence.0);
            if body.len() < 4 {
                return Ok(RequestOutcome::Handled);
            }
            let window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            // Xorg ProcXIGetSelectedEvents: one xXIEventMask per device
            // in device-id order, each trimmed to the words that hold set
            // bits. deviceid/mask_len follow the client byte order; the
            // mask itself is a bit array (bit n in byte n/8), written as
            // little-endian bytes for every client.
            let mut selected: Vec<(u16, u64)> = state
                .clients
                .get(&client_id.0)
                .map(|client| {
                    client
                        .xi2_masks
                        .iter()
                        .filter(|&(&(win, _), &mask)| win == window && mask != 0)
                        .map(|(&(_, dev), &mask)| (dev, mask))
                        .collect()
                })
                .unwrap_or_default();
            selected.sort_unstable_by_key(|&(dev, _)| dev);
            let mut masks = Vec::new();
            for &(dev, mask) in &selected {
                let mask_len: u16 = if mask >> 32 == 0 { 1 } else { 2 };
                x11::write_u16(byte_order, &mut masks, dev);
                x11::write_u16(byte_order, &mut masks, mask_len);
                masks.extend_from_slice(&mask.to_le_bytes()[..4 * usize::from(mask_len)]);
            }
            let num_masks = u16::try_from(selected.len()).unwrap_or(u16::MAX);
            let mut reply = x11::fixed_reply(
                byte_order,
                sequence,
                0,
                x11::checked_units(masks.len())? as u32,
            );
            x11::write_u16(byte_order, &mut reply, num_masks);
            reply.extend_from_slice(&[0; 22]);
            reply.extend_from_slice(&masks);
            buf.extend_from_slice(&reply);
        }
        40 => {
            // XIQueryPointer: GDK calls this to fetch the current
            // pointer position for gesture-drag anchors (caja-desktop
            // marquee, mate-panel applet drags). Previously hardcoded
            // every coordinate to 0, so every drag started from (0,0)
            // regardless of the actual cursor position — caja then
            // rubber-banded from screen origin to the click point on
            // any single click. Wire the real cursor position through
            // from the backend.
            // XIQueryPointer body layout (per xinput.xml):
            //   bytes [0..4] window (WINDOW)
            //   bytes [4..6] deviceid (u16)
            //   bytes [6..8] pad
            // Previously this read body[4..8], which is deviceid + pad
            // — i.e. always interpreted as some sentinel xid (e.g.
            // `0xfd820002`, `0xffff0002`). Then
            // `window_absolute_position` couldn't find the bogus xid
            // and returned `(0, 0)`, so the win-relative coords came
            // out equal to the root-absolute coords. GTK popups in
            // file managers (Thunar, Caja) place themselves at
            // `(window_origin + win_xy + menu_offset)` — with our
            // wrong reply they ended up offset by the window's own
            // origin, and rubber-band drags anchored at root (0,0)
            // instead of at the click point.
            let queried_window = if body.len() >= 4 {
                ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
            } else {
                ROOT_WINDOW
            };
            let pointer = backend.query_pointer(origin).ok();
            let (root_x_int, root_y_int, mask, same_screen) =
                if let Some(p) = pointer.filter(|p| p.same_screen) {
                    (p.win_x, p.win_y, p.mask, 1u8)
                } else {
                    (0, 0, 0, 0u8)
                };
            let (origin_x, origin_y) = state.resources.window_absolute_position(queried_window);
            let win_x_int =
                i16::try_from(i32::from(root_x_int).saturating_sub(origin_x)).unwrap_or(i16::MAX);
            let win_y_int =
                i16::try_from(i32::from(root_y_int).saturating_sub(origin_y)).unwrap_or(i16::MAX);
            let child = state
                .direct_child_at(queried_window, win_x_int, win_y_int)
                .unwrap_or(ResourceId(0));
            debug!(
                "client {} #{} XIQueryPointer window=0x{:x} -> root=({},{}) win=({},{}) child=0x{:x} mask=0x{:x}",
                client_id.0,
                sequence.0,
                queried_window.0,
                root_x_int,
                root_y_int,
                win_x_int,
                win_y_int,
                child.0,
                mask,
            );
            // Reply layout (length = 6 units = 24 bytes after the
            // 8-byte header — total 32 + 24 = 56). FP1616 coords are
            // (i32::from(coord) << 16) as u32. Per xinput.xml the
            // BOOL `same_screen` is at offset 32 (NOT in the `pad0`
            // slot at offset 1 — confirmed via x11trace 2026-05-15).
            // GTK uses `same_screen` to pick popup-placement code
            // paths; encoding it as pad0 left every client reading
            // `same_screen=false` and routing through the "different
            // screen" branch, which made Thunar's right-click popup
            // appear at root+window_origin instead of near the click.
            // XI2 button state: fold the KeyButMask button bits into the
            // XI2 button bitmask (button N -> bit N). muffin's
            // `_NET_WM_MOVERESIZE` move grab queries the pointer to learn
            // whether the initiating button is still held — an empty mask
            // makes it abort the move and the window never budges.
            let mut button_mask: u32 = 0;
            for (keybut_bit, button) in [
                (0x0100u16, 1u32),
                (0x0200, 2),
                (0x0400, 3),
                (0x0800, 4),
                (0x1000, 5),
            ] {
                if mask & keybut_bit != 0 {
                    button_mask |= 1 << button;
                }
            }
            // length 7 units: 6 coord words + the same_screen/buttons_len/
            // ModifierInfo/GroupInfo block + a 1-unit button bitmask.
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 7);
            x11::write_u32(byte_order, &mut reply, ROOT_WINDOW.0);
            x11::write_u32(byte_order, &mut reply, child.0);
            x11::write_u32(byte_order, &mut reply, (i32::from(root_x_int) << 16) as u32);
            x11::write_u32(byte_order, &mut reply, (i32::from(root_y_int) << 16) as u32);
            x11::write_u32(byte_order, &mut reply, (i32::from(win_x_int) << 16) as u32);
            x11::write_u32(byte_order, &mut reply, (i32::from(win_y_int) << 16) as u32);
            reply.push(same_screen); // byte 32: same_screen (BOOL)
            reply.push(0); // byte 33: pad
            x11::write_u16(byte_order, &mut reply, 1); // bytes 34-35: buttons_len (1 unit)
            // ModifierInfo: 4× CARD32 = base / latched / locked /
            // effective. `mask` carries the X11 KeyButMask snapshot
            // from the backend: MODIFIERS in the low byte (Shift=0x1 …
            // Mod5=0x80), buttons at 0x100+. Xorg fills `rep.mods`
            // from the paired MASTER_KEYBOARD's XKB state
            // (Xi/xiquerypointer.c:120,139) — base_mods must carry the
            // modifier bits WITHOUT the button bits. Pre-fix base was
            // hardcoded 0: cinnamon's alt-tab switcher polls
            // `global.get_pointer()` (GDK → XIQueryPointer) right
            // after pushModal to check Alt is still held; reading 0 it
            // took the modifier-already-released branch
            // (_activateSelected + destroy) and the switcher popup
            // never appeared. `effective` keeps the full KeyButMask
            // (mods + button bits) — GDK reads effective and the
            // button bits are harmlessly idempotent with the XI2
            // buttons array below.
            let mod_bits = u32::from(mask & 0x00ff);
            x11::write_u32(byte_order, &mut reply, mod_bits); // base_mods
            x11::write_u32(byte_order, &mut reply, 0); // latched_mods
            x11::write_u32(byte_order, &mut reply, 0); // locked_mods
            x11::write_u32(byte_order, &mut reply, u32::from(mask)); // effective_mods
            reply.extend_from_slice(&[0u8; 4]); // group info
            // XI2 button mask is a raw byte array indexed by
            // `XIMaskIsSet(ptr, btn) = ptr[btn>>3] & (1 << (btn & 7))`
            // (X11/extensions/XI2.h) — byte[0] holds buttons 0-7. It is
            // NOT byte-swapped per client order (Xorg leaves the trailing
            // mask bytes untouched in its reply swap), so emit
            // little-endian unconditionally.
            reply.extend_from_slice(&button_mask.to_le_bytes());
            buf.extend_from_slice(&reply);
        }
        // XI2 grab opcodes (51-55). yserver wires these into the
        // existing core X11 grab state (`state.active_pointer_grab`,
        // `state.button_grabs`,
        // `state.active_keyboard_grab`, `state.key_grabs`) which the
        // pointer/key fanout already honours. The XI2 mask + per-
        // device routing isn't fully implemented — every grab is
        // mapped to the requested master slot or exact slave-device grab
        // slot according to the registered device role. That matches what
        // GTK relies on in practice (its uses of XIGrabDevice are equivalent
        // to XGrabPointer/XGrabKeyboard for the master devices).
        //
        // Pre-fix all five handlers were no-ops that just sent
        // Success replies — GTK then thought it owned the device but
        // events still went to the normal pointer-window, so popups
        // dismissed on the first stray motion event and `gtk_window_
        // present` looped re-mapping the popup at ~50 Hz (visible as
        // brisk-menu's MapWindow remap storm pre-`MapWindow`
        // no-op fix, and as subtle popup misbehavior elsewhere).
        //
        // Match Xorg `Xi/exevents.c::DeviceGrabDevice` /
        // `ProcXIPassiveGrabDevice` for state-update intent. The
        // status byte at reply offset 8 is always Success(0); a
        // strict implementation would return AlreadyGrabbed(1) /
        // NotViewable(3) / etc., but Xorg's permissive behaviour
        // (set the grab and let later events sort it out) keeps GTK
        // happy across pause/resume sleeps.
        51 => {
            // XIGrabDevice reply: 32 bytes (header + status at offset
            // 8 + 23 pad). Wire body: window(4) + time(4) + cursor(4)
            // + deviceid(2) + mode(1) + paired_device_mode(1) +
            // owner_events(1) + pad(1) + mask_len(2) + ...
            // XIGrabDevice body: window(4) time(4) cursor(4)
            // deviceid(2) mode(1) paired(1) owner_events(1) pad(1)
            // mask_len(2) mask...
            let (grab_window, time, cursor, deviceid, owner_events) = if body.len() >= 17 {
                (
                    u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
                    u16::from_le_bytes([body[12], body[13]]),
                    body[16] != 0,
                )
            } else {
                (0, 0, 0, 0, false)
            };
            // The grab's XI2 event-type mask follows mask_len at body[18..20].
            // Keep the first two words represented by ActivePointerGrab's
            // u64 mask; these cover all pointer event types currently routed.
            let grab_xi2_mask = body
                .get(18..20)
                .map_or(0, |length| {
                    usize::from(u16::from_le_bytes([length[0], length[1]]))
                })
                .saturating_mul(4)
                .min(8)
                .checked_add(20)
                .and_then(|end| body.get(20..end))
                .map_or(0, |mask_bytes| {
                    let mut mask = [0u8; 8];
                    mask[..mask_bytes.len()].copy_from_slice(mask_bytes);
                    u64::from_le_bytes(mask)
                });
            let role = match state.xi_devices.role(deviceid) {
                Some(role) => role,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR,
                        u32::from(deviceid),
                        51,
                        XI2_MAJOR_OPCODE,
                    );
                }
            };
            let is_keyboard = matches!(
                role,
                crate::xinput::XiDeviceRole::MasterKeyboard
                    | crate::xinput::XiDeviceRole::SlaveKeyboard
            );
            let is_master = matches!(
                role,
                crate::xinput::XiDeviceRole::MasterKeyboard
                    | crate::xinput::XiDeviceRole::MasterPointer
            );
            // Xorg `dix/events.c:5240` (GrabDevice): a device already
            // grabbed by ANOTHER client → AlreadyGrabbed(1); do NOT
            // overwrite the grab. Same-client re-grab still replaces
            // (Xorg SameClient path, unchanged below). #94 Cinnamon
            // click-swallow: muffin `XIGrabDevice`-stole the master
            // pointer from Steam mid-click (both device=2), so the
            // ButtonRelease was delivered to muffin and Steam's click
            // never completed (cinnamon.xtrace conn 026 vs 105). The
            // pointer slot covers implicit / passive-activated / explicit
            // grabs alike — exactly Xorg's single `deviceGrab.grab`.
            let grab_status: u8 = if is_keyboard && is_master {
                u8::from(
                    state
                        .active_keyboard_grab
                        .is_some_and(|g| g.owner != client_id),
                )
            } else if is_keyboard {
                u8::from(
                    state
                        .xi2_keyboard_grabs
                        .get(&deviceid)
                        .is_some_and(|g| g.owner != client_id),
                )
            } else if is_master {
                u8::from(
                    state
                        .active_pointer_grab
                        .is_some_and(|grab| grab.owner != client_id),
                )
            } else {
                u8::from(
                    state
                        .xi2_pointer_grabs
                        .get(&deviceid)
                        .is_some_and(|grab| grab.owner != client_id),
                )
            };
            if grab_status == 0 {
                // Capture the previous grab window (if any, this client's)
                // BEFORE we overwrite. Synthesised crossings need to send
                // Leave on the previous grab window when transitioning to
                // a different one. GTK3 popups grab the input-shadow
                // window first, then re-grab the visible popup — without
                // the Leave on the shadow window GTK3's menu-tracking
                // never engages on the visible popup.
                let prev_pointer_grab_window: Option<ResourceId> = if is_keyboard {
                    None
                } else if is_master {
                    state
                        .active_pointer_grab
                        .filter(|g| g.owner == client_id)
                        .map(|g| g.grab_window)
                } else {
                    state
                        .xi2_pointer_grabs
                        .get(&deviceid)
                        .filter(|g| g.owner == client_id)
                        .map(|g| g.grab_window)
                };
                let prev_keyboard_grab_window: Option<ResourceId> = if is_keyboard && is_master {
                    state
                        .active_keyboard_grab
                        .filter(|g| g.owner == client_id)
                        .map(|g| g.grab_window)
                } else {
                    state
                        .xi2_keyboard_grabs
                        .get(&deviceid)
                        .filter(|g| g.owner == client_id)
                        .map(|g| g.grab_window)
                };
                if is_keyboard {
                    let grab = crate::server::ActiveKeyboardGrab {
                        owner: client_id,
                        grab_window: ResourceId(grab_window),
                        source: crate::server::ActiveKeyboardGrabSource::Explicit,
                        owner_events,
                        via_xi2: true,
                        xi2_mask: u32::try_from(grab_xi2_mask & u64::from(u32::MAX))
                            .expect("first XI2 mask word fits CARD32"),
                    };
                    if is_master {
                        state.active_keyboard_grab = Some(grab);
                    } else {
                        state.xi2_keyboard_grabs.insert(deviceid, grab);
                        let _ = state.detach_xi2_slave(deviceid);
                    }
                } else {
                    let grab = crate::server::ActivePointerGrab {
                        owner: client_id,
                        grab_window: ResourceId(grab_window),
                        event_mask: 0xFFFF, // core mask is unused for XI2 grabs
                        cursor: ResourceId(cursor),
                        time,
                        owner_events,
                        via_xi2: true,
                        implicit: false,
                        passive: false,
                        xi2_mask: grab_xi2_mask,
                    };
                    if is_master {
                        state.set_pointer_grab(grab);
                    } else {
                        state.xi2_pointer_grabs.insert(deviceid, grab);
                        let _ = state.detach_xi2_slave(deviceid);
                    }
                }
                // Core↔XI bridge — Xorg `ActivateKeyboardGrab` /
                // `ActivatePointerGrab` end with `CheckGrabForSyncs`
                // (dix/events.c:1424): a SYNCHRONOUS grab freezes this
                // device, an ASYNCHRONOUS grab THAWS it — including a
                // freeze left behind by a sync passive-grab activation —
                // and `ComputeFreezes` replays the withheld queues. The
                // core GrabKeyboard/GrabPointer handlers already do this;
                // pre-fix the XI2 path skipped it, so muffin's alt-tab
                // (sync passive keybinding grab → ASYNC XIGrabDevice
                // pushModal → XIUngrabDevice) left the keyboard
                // FrozenNoEvent forever: one alt-tab killed all key input
                // (the XIAllowEvents muffin sends after the ungrab is
                // correctly a no-op — its grab is already gone).
                // Wire: body[14]=grab_mode, body[15]=paired_device_mode;
                // XI2 GrabModeSync=0 / GrabModeAsync=1.
                let grab_mode = body.get(14).copied().unwrap_or(1);
                let paired_mode = body.get(15).copied().unwrap_or(1);
                crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
                    state,
                    deviceid,
                    client_id,
                    grab_mode == 0,
                    paired_mode == 0,
                );
                if grab_mode != 0 {
                    // The withheld sync-passive replay candidate was already
                    // delivered to the grab owner; an async grab forecloses
                    // Replay (Xorg drops the stored event on thaw).
                    state.xi1_frozen.entry(deviceid).or_default().stored = None;
                }
                debug!(
                    "client {} #{} XIGrabDevice window=0x{:x} deviceid={} cursor=0x{:x} -> Success",
                    client_id.0, sequence.0, grab_window, deviceid, cursor
                );
                // Synthesised XI2 crossings on grab activation. Matches
                // Xorg `Xi/exevents.c::ActivateKeyboardGrab` /
                // `ActivatePointerGrab` (which call `DoEnterLeaveEvents`
                // with `NotifyGrab`). Without these GTK3 popup state
                // machines never engage their hover/click tracking — the
                // menu is visible but items don't highlight or activate
                // on click. Captured in `mate-xorg.xtrace` as the pair
                // `XI_Leave(mode=Grab, detail=Nonlinear)` on the previous
                // grab window followed by `XI_Enter(mode=Grab,
                // detail=Nonlinear)` on the new grab window; keyboard
                // grabs additionally get `XI_FocusIn(mode=Grab,
                // detail=Nonlinear)`. NotifyGrab = 1, NotifyNonlinear = 3.
                let pointer_xy = backend
                    .query_pointer(origin)
                    .ok()
                    .map(|p| (p.win_x, p.win_y))
                    .unwrap_or((0, 0));
                let (root_x, root_y) = pointer_xy;
                let server_time = state.timestamp_now();
                // Emit one XI2 crossing (Enter/Leave/FocusIn/FocusOut) on a
                // single window with an explicit detail code, mode
                // NotifyGrab. The keyboard-focus path drives this directly;
                // the pointer path drives it from a crossing chain so the
                // detail codes — and the `from == to` no-op — match Xorg's
                // `DoEnterLeaveEvents`.
                let emit_xi_crossing = |state: &mut ServerState,
                                        evtype: u16,
                                        target_window: ResourceId,
                                        detail: u8| {
                    let (origin_x, origin_y) =
                        state.resources.window_absolute_position(target_window);
                    let event_x = i16::try_from(i32::from(root_x).saturating_sub(origin_x))
                        .unwrap_or(i16::MAX);
                    let event_y = i16::try_from(i32::from(root_y).saturating_sub(origin_y))
                        .unwrap_or(i16::MAX);
                    let focus =
                        !matches!(evtype, 9 | 10) && state.crossing_has_focus(target_window);
                    let _dropped =
                        fanout_event_to_clients(state, &[client_id], |out, seq, order| {
                            x11::encode_xi2_crossing_event(
                                out,
                                order,
                                seq,
                                XI2_MAJOR_OPCODE,
                                evtype,
                                deviceid,
                                server_time,
                                ROOT_WINDOW,
                                target_window,
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                0,
                                1, // mode = NotifyGrab
                                detail,
                                deviceid,
                                focus,
                            );
                        });
                };
                if is_keyboard {
                    // Keyboard grab → focus transitions (Xorg
                    // `ActivateKeyboardGrab` → `DoFocusEvents`). Unchanged.
                    if let Some(prev) = prev_keyboard_grab_window
                        && prev != ResourceId(grab_window)
                    {
                        emit_xi_crossing(state, 10, prev, 3); // XI_FocusOut
                    }
                    emit_xi_crossing(state, 9, ResourceId(grab_window), 3); // XI_FocusIn
                } else {
                    // Pointer grab → Xorg `ActivatePointerGrab` →
                    // `DoEnterLeaveEvents(from, grab_window, NotifyGrab)`
                    // (dix/events.c). `from` is the previous grab window
                    // when replacing an active grab, else the window the
                    // pointer currently sits in (the sprite, resolved from
                    // the live position). `compute_crossing_chain`
                    // early-returns when `from == grab_window`, so grabbing
                    // the window already under the pointer emits nothing —
                    // matching Xorg. The pre-fix code emitted an
                    // unconditional `XI_Enter(NotifyGrab)` on the grab
                    // window, which desynced cinnamon's XEmbed-systray
                    // click-forward whenever muffin grabbed its own
                    // full-screen stage (pamac tray unclickable, 2026-06-22).
                    let from = prev_pointer_grab_window.unwrap_or_else(|| {
                        state
                            .root_pointer_target_at(root_x, root_y)
                            .map_or(ResourceId(grab_window), |h| h.0)
                    });
                    let chain = crate::crossings::implicit_grab_crossings(
                        state,
                        from,
                        ResourceId(grab_window),
                    );
                    for e in chain {
                        let evtype: u16 = match e.kind {
                            crate::crossings::CrossingKind::Enter => 7,
                            crate::crossings::CrossingKind::Leave => 8,
                        };
                        emit_xi_crossing(state, evtype, e.window, e.detail);
                    }
                }
            } // end: mutate grab state + emit crossings only when granted
            // XIGrabDevice reply carries the grab status at offset 8
            // (0 = Success, 1 = AlreadyGrabbed — Xorg dix/events.c:5240).
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            let mut grab_reply_body = [0u8; 24];
            grab_reply_body[0] = grab_status;
            reply.extend_from_slice(&grab_reply_body);
            buf.extend_from_slice(&reply);
        }
        52 => {
            // XIUngrabDevice body: time(4) + deviceid(2) + pad(2).
            let deviceid = if body.len() >= 6 {
                u16::from_le_bytes([body[4], body[5]])
            } else {
                0
            };
            let role = match state.xi_devices.role(deviceid) {
                Some(role) => role,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR,
                        u32::from(deviceid),
                        52,
                        XI2_MAJOR_OPCODE,
                    );
                }
            };
            let is_keyboard = matches!(
                role,
                crate::xinput::XiDeviceRole::MasterKeyboard
                    | crate::xinput::XiDeviceRole::SlaveKeyboard
            );
            let is_master = matches!(
                role,
                crate::xinput::XiDeviceRole::MasterKeyboard
                    | crate::xinput::XiDeviceRole::MasterPointer
            );
            // Capture grab_window BEFORE clearing — we synthesize the
            // matching XI2 crossing with `NotifyUngrab` on the way out
            // so the client's grab state machine pairs cleanly with
            // its earlier NotifyGrab. Matches Xorg
            // `Xi/exevents.c::DeactivateKeyboardGrab` /
            // `DeactivatePointerGrab`.
            let grab_window_for_event: Option<ResourceId> = if is_keyboard && is_master {
                state
                    .active_keyboard_grab
                    .filter(|g| g.owner == client_id)
                    .map(|g| g.grab_window)
            } else if is_keyboard {
                state
                    .xi2_keyboard_grabs
                    .get(&deviceid)
                    .filter(|g| g.owner == client_id)
                    .map(|g| g.grab_window)
            } else if is_master {
                state
                    .active_pointer_grab
                    .filter(|grab| grab.owner == client_id)
                    .map(|grab| grab.grab_window)
            } else {
                state
                    .xi2_pointer_grabs
                    .get(&deviceid)
                    .filter(|grab| grab.owner == client_id)
                    .map(|grab| grab.grab_window)
            };
            if grab_window_for_event.is_some() {
                if is_keyboard && is_master {
                    state.active_keyboard_grab = None;
                } else if is_keyboard {
                    state.xi2_keyboard_grabs.remove(&deviceid);
                } else if is_master {
                    state.clear_pointer_grab();
                } else {
                    state.xi2_pointer_grabs.remove(&deviceid);
                }
                state.xi1_frozen.entry(deviceid).or_default().stored = None;
                crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(
                    state, deviceid, client_id,
                );
                if !is_master {
                    state.reattach_xi2_slave(deviceid);
                }
                if !is_keyboard {
                    // Clear any grab-cursor sprite override, as
                    // `deactivate_core_pointer_grab` does — a core
                    // XGrabPointer(cursor) torn down via XIUngrabDevice
                    // must not strand the grab cursor on the sprite.
                    let _ = backend.set_grab_cursor(None, None);
                }
            }
            if let Some(grab_window) = grab_window_for_event {
                let pointer_xy = backend
                    .query_pointer(origin)
                    .ok()
                    .map(|p| (p.win_x, p.win_y))
                    .unwrap_or((0, 0));
                let (root_x, root_y) = pointer_xy;
                let server_time = state.timestamp_now();
                let emit_xi_crossing = |state: &mut ServerState,
                                        evtype: u16,
                                        target_window: ResourceId,
                                        detail: u8| {
                    let (origin_x, origin_y) =
                        state.resources.window_absolute_position(target_window);
                    let event_x = i16::try_from(i32::from(root_x).saturating_sub(origin_x))
                        .unwrap_or(i16::MAX);
                    let event_y = i16::try_from(i32::from(root_y).saturating_sub(origin_y))
                        .unwrap_or(i16::MAX);
                    let focus =
                        !matches!(evtype, 9 | 10) && state.crossing_has_focus(target_window);
                    let _dropped =
                        fanout_event_to_clients(state, &[client_id], |out, seq, order| {
                            x11::encode_xi2_crossing_event(
                                out,
                                order,
                                seq,
                                XI2_MAJOR_OPCODE,
                                evtype,
                                deviceid,
                                server_time,
                                ROOT_WINDOW,
                                target_window,
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                0,
                                2, // mode = NotifyUngrab
                                detail,
                                deviceid,
                                focus,
                            );
                        });
                };
                if is_keyboard {
                    // Keyboard ungrab → focus restore. Unchanged.
                    emit_xi_crossing(state, 10, grab_window, 3); // XI_FocusOut
                } else {
                    // Pointer ungrab → Xorg `DeactivatePointerGrab` →
                    // `DoEnterLeaveEvents(grab_window, sprite, NotifyUngrab)`
                    // (dix/events.c). Symmetric with the activation
                    // chain: `from == to` (sprite still inside the grab
                    // window) emits nothing.
                    let to = state
                        .root_pointer_target_at(root_x, root_y)
                        .map_or(grab_window, |h| h.0);
                    let chain = crate::crossings::implicit_grab_crossings(state, grab_window, to);
                    for e in chain {
                        let evtype: u16 = match e.kind {
                            crate::crossings::CrossingKind::Enter => 7,
                            crate::crossings::CrossingKind::Leave => 8,
                        };
                        emit_xi_crossing(state, evtype, e.window, e.detail);
                    }
                }
            }
            debug!(
                "client {} #{} XIUngrabDevice deviceid={}",
                client_id.0, sequence.0, deviceid
            );
            return Ok(RequestOutcome::Handled);
        }
        53 => {
            // XIAllowEvents body (per X11/extensions/XI2proto.h
            // `xXIAllowEventsReq`): time(4) + deviceid(2) + mode(1) +
            // pad(1). XI 2.2 extends with touchid(4) + grab_window(4)
            // for touch grabs; we don't implement touch, so the
            // extension fields are ignored.
            //
            // mutter/muffin/cinnamon install click-to-focus passive
            // button grabs (sync) and keybinding key grabs (sync); on each
            // press they examine it then call XIAllowEvents to thaw/replay.
            // The mode (XIAsyncDevice/XISyncDevice/XIReplayDevice/…) +
            // deviceid map onto a core AllowSome mode — see
            // `xi2_allow_mode_to_core` — and run through the shared
            // `apply_allow_events` below.
            let (deviceid, mode) = if body.len() >= 8 {
                (u16::from_le_bytes([body[4], body[5]]), body[6])
            } else {
                (0, 0)
            };
            let role = match state.xi_devices.role(deviceid) {
                Some(role) => role,
                None => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR,
                        u32::from(deviceid),
                        53,
                        XI2_MAJOR_OPCODE,
                    );
                }
            };
            let is_keyboard = matches!(
                role,
                crate::xinput::XiDeviceRole::MasterKeyboard
                    | crate::xinput::XiDeviceRole::SlaveKeyboard
            );
            let is_master = matches!(
                role,
                crate::xinput::XiDeviceRole::MasterKeyboard
                    | crate::xinput::XiDeviceRole::MasterPointer
            );
            debug!(
                "client {} #{} XIAllowEvents deviceid={} mode={}",
                client_id.0, sequence.0, deviceid, mode
            );
            let time = if body.len() >= 4 {
                u32::from_le_bytes([body[0], body[1], body[2], body[3]])
            } else {
                0
            };
            // Map the XI2 mode + target deviceid onto the equivalent core
            // AllowSome mode and run the SHARED `apply_allow_events`. This
            // used to be a partial reimplementation that diverged from the
            // core path and caused recurring freezes (desktop rubber-band =
            // Async/Replay thaw gap; Cinnamon input freeze = XISyncDevice
            // treated as a no-op). Touch modes (XIAcceptTouch/XIRejectTouch)
            // are unsupported → no-op.
            if is_master {
                if let Some(core_mode) = xi2_allow_mode_to_core(mode, is_keyboard) {
                    return apply_allow_events(
                        state, backend, client_id, sequence, core_mode, time,
                    );
                }
            } else {
                return apply_xi2_allow_events_for_slave(
                    state,
                    backend,
                    client_id,
                    sequence,
                    deviceid,
                    is_keyboard,
                    mode,
                    time,
                );
            }
            return Ok(RequestOutcome::Handled);
        }
        54 => {
            // XIPassiveGrabDevice body (per X11/extensions/XI2proto.h
            // `xXIPassiveGrabDeviceReq`): time(4) + grab_window(4) +
            // cursor(4) + detail(4) + deviceid(2) + num_modifiers(2)
            // + mask_len(2) + grab_type(1) + grab_mode(1) +
            // paired_device_mode(1) + owner_events(1) + pad1(2) +
            // mask(mask_len*4) + modifiers(num_modifiers*4).
            //
            // GrabType: Button=0, Keycode=1, Enter=2, FocusIn=3,
            // TouchBegin=4. Button and Keycode map onto core X11
            // PassiveButtonGrab / KeyGrab; the other three are no-ops
            // for now (yserver doesn't synthesise core X11 Enter/
            // FocusIn passive grabs).
            let (
                grab_window,
                detail,
                deviceid,
                num_modifiers,
                mask_len,
                grab_type,
                grab_mode,
                paired_device_mode,
                owner_events,
            ) = if body.len() >= 25 {
                (
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    u32::from_le_bytes([body[12], body[13], body[14], body[15]]),
                    u16::from_le_bytes([body[16], body[17]]),
                    u16::from_le_bytes([body[18], body[19]]),
                    u16::from_le_bytes([body[20], body[21]]) as usize,
                    body[22],
                    body[23],
                    body[24],
                    body.get(25).copied().unwrap_or(0) != 0,
                )
            } else {
                (0, 0, 0, 0, 0, 0xff, 1, 1, false)
            };
            if matches!(grab_type, 0 | 1) {
                let role = state.xi_devices.role(deviceid);
                let compatible = match grab_type {
                    0 => matches!(
                        role,
                        Some(
                            crate::xinput::XiDeviceRole::MasterPointer
                                | crate::xinput::XiDeviceRole::SlavePointer
                        )
                    ),
                    1 => matches!(
                        role,
                        Some(
                            crate::xinput::XiDeviceRole::MasterKeyboard
                                | crate::xinput::XiDeviceRole::SlaveKeyboard
                        )
                    ),
                    _ => true,
                };
                if !compatible {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR,
                        u32::from(deviceid),
                        54,
                        XI2_MAJOR_OPCODE,
                    );
                }
            }
            // Modifiers tail starts after the header (28) and the mask
            // (mask_len * 4 bytes).
            let mods_start = 28 + mask_len * 4;
            // First mask word: event types 0..=31.
            let grab_xi2_mask = if mask_len > 0 {
                body.get(28..32)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            } else {
                0
            };
            let mut modifier_masks: Vec<u16> = Vec::with_capacity(num_modifiers.max(1).into());
            if num_modifiers == 0 {
                // "no modifiers" — single grab with modifier-mask 0.
                modifier_masks.push(0);
            } else {
                for i in 0..usize::from(num_modifiers) {
                    let off = mods_start + i * 4;
                    if let Some(slice) = body.get(off..off + 4) {
                        let raw = u32::from_le_bytes(slice.try_into().unwrap());
                        // XI2 `Any` = bit 31; map to core X11 AnyModifier (0x8000).
                        #[allow(clippy::cast_possible_truncation)]
                        let m = if raw & 0x8000_0000 != 0 {
                            0x8000u16
                        } else {
                            (raw & 0xFFFF) as u16
                        };
                        modifier_masks.push(m);
                    }
                }
            }
            match grab_type {
                0 => {
                    // Button. detail = button number (0 == AnyButton).
                    let button = u8::try_from(detail).unwrap_or(0);
                    for modifiers in &modifier_masks {
                        state.button_grabs.retain(|g| {
                            !(g.owner == client_id
                                && g.device_id == deviceid
                                && g.grab_window.0 == grab_window
                                && g.button == button
                                && g.modifiers == *modifiers)
                        });
                        state.button_grabs.push(crate::server::PassiveButtonGrab {
                            device_id: deviceid,
                            owner: client_id,
                            grab_window: ResourceId(grab_window),
                            button,
                            modifiers: *modifiers,
                            owner_events,
                            event_mask: 0xFFFF_FFFF,
                            pointer_mode: grab_mode,
                            keyboard_mode: 1,
                            confine_to: ResourceId(0),
                            via_xi2: true,
                        });
                    }
                }
                1 => {
                    // Keycode. detail = keycode (0 == AnyKey).
                    let keycode = u8::try_from(detail).unwrap_or(0);
                    for modifiers in &modifier_masks {
                        state.key_grabs.retain(|g| {
                            !(g.owner == client_id
                                && g.device_id == deviceid
                                && g.grab_window.0 == grab_window
                                && g.keycode == keycode
                                && g.modifiers == *modifiers)
                        });
                        state.key_grabs.push(crate::server::KeyGrab {
                            device_id: deviceid,
                            owner: client_id,
                            grab_window: ResourceId(grab_window),
                            keycode,
                            modifiers: *modifiers,
                            owner_events: false,
                            pointer_mode: paired_device_mode,
                            keyboard_mode: grab_mode,
                            via_xi2: true,
                            xi2_mask: grab_xi2_mask,
                        });
                    }
                }
                _ => {
                    // Enter/FocusIn/TouchBegin — yserver has no
                    // matching machinery yet; record the request in
                    // the debug log so a real repro shows up.
                    debug!(
                        "client {} #{} XIPassiveGrabDevice unsupported grab_type={} \
                         (Enter/FocusIn/TouchBegin)",
                        client_id.0, sequence.0, grab_type
                    );
                }
            }
            debug!(
                "client {} #{} XIPassiveGrabDevice window=0x{:x} detail=0x{:x} deviceid={} \
                 grab_type={} num_modifiers={} -> Success",
                client_id.0, sequence.0, grab_window, detail, deviceid, grab_type, num_modifiers,
            );
            // Reply: num_modifiers(2) + pad(22). With 0 failed
            // modifiers the client treats every requested combination
            // as grabbed.
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            reply.extend_from_slice(&[0u8; 24]);
            buf.extend_from_slice(&reply);
        }
        55 => {
            // XIPassiveUngrabDevice body: grab_window(4) + detail(4)
            // + deviceid(2) + num_modifiers(2) + grab_type(1) +
            // pad(3) + modifiers.
            let (grab_window, detail, device_id, num_modifiers, grab_type) = if body.len() >= 13 {
                (
                    u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    u16::from_le_bytes([body[8], body[9]]),
                    u16::from_le_bytes([body[10], body[11]]),
                    body[12],
                )
            } else {
                (0, 0, 0, 0, 0xff)
            };
            if matches!(grab_type, 0 | 1) {
                let role = state.xi_devices.role(device_id);
                let compatible = match grab_type {
                    0 => matches!(
                        role,
                        Some(
                            crate::xinput::XiDeviceRole::MasterPointer
                                | crate::xinput::XiDeviceRole::SlavePointer
                        )
                    ),
                    1 => matches!(
                        role,
                        Some(
                            crate::xinput::XiDeviceRole::MasterKeyboard
                                | crate::xinput::XiDeviceRole::SlaveKeyboard
                        )
                    ),
                    _ => true,
                };
                if !compatible {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI2_FIRST_ERROR,
                        u32::from(device_id),
                        55,
                        XI2_MAJOR_OPCODE,
                    );
                }
            }
            let mods_start = 16;
            let mut modifier_masks: Vec<u16> = Vec::with_capacity(num_modifiers.max(1).into());
            if num_modifiers == 0 {
                modifier_masks.push(0);
            } else {
                for i in 0..usize::from(num_modifiers) {
                    let off = mods_start + i * 4;
                    if let Some(slice) = body.get(off..off + 4) {
                        let raw = u32::from_le_bytes(slice.try_into().unwrap());
                        #[allow(clippy::cast_possible_truncation)]
                        let m = if raw & 0x8000_0000 != 0 {
                            0x8000u16
                        } else {
                            (raw & 0xFFFF) as u16
                        };
                        modifier_masks.push(m);
                    }
                }
            }
            match grab_type {
                0 => {
                    let button = u8::try_from(detail).unwrap_or(0);
                    state.button_grabs.retain(|g| {
                        !(g.owner == client_id
                            && g.device_id == device_id
                            && g.grab_window.0 == grab_window
                            && (g.button == button || button == 0)
                            && modifier_masks
                                .iter()
                                .any(|m| g.modifiers == *m || *m == 0x8000))
                    });
                }
                1 => {
                    let keycode = u8::try_from(detail).unwrap_or(0);
                    state.key_grabs.retain(|g| {
                        !(g.owner == client_id
                            && g.device_id == device_id
                            && g.grab_window.0 == grab_window
                            && (g.keycode == keycode || keycode == 0)
                            && modifier_masks
                                .iter()
                                .any(|m| g.modifiers == *m || *m == 0x8000))
                    });
                }
                _ => {}
            }
            debug!(
                "client {} #{} XIPassiveUngrabDevice window=0x{:x} detail=0x{:x} grab_type={}",
                client_id.0, sequence.0, grab_window, detail, grab_type
            );
            return Ok(RequestOutcome::Handled);
        }
        // XI 1.x ListInputDevices (minor 2). It selects the same live
        // registry entries as XIQueryDevice. A previous empty-list stub
        // crashed Chromium/Electron: Ozone-X11 cross-checks XI1's
        // ListInputDevices against XI2's XIQueryDevice and fatal-CHECKs
        // when XI2 has a master pointer but XI1 reports zero devices.
        2 => {
            debug!("client {} #{} XListInputDevices", client_id.0, sequence.0);
            // Keep the same descriptor order as XIQueryDevice selector 0.
            // XI1 has no enabled field. Match Xorg's None type for masters
            // and XTEST devices while preserving each physical facet's type.
            const PHYSICAL_POINTER_AXES: [(i32, i32); 4] = [(-1, -1), (-1, -1), (-1, 0), (-1, 0)];
            const CORE_POINTER_AXES: [(i32, i32); 2] = [(-1, -1), (-1, -1)];
            // XI1 copies `d->xinput_type` directly to the type atom
            // (`Xi/listdev.c:171-173`). The server-created master and XTEST
            // devices keep calloc's initial None value
            // (`dix/devices.c:259-261`); physical types are assigned by the
            // input driver (`Xi/extinit.c:1207-1210`).
            let no_type_atom = x11::AtomId(0);
            let mouse_atom = state.atoms.intern(crate::xinput::XI_ATOM_MOUSE, true);
            let keyboard_atom = state.atoms.intern(crate::xinput::XI_ATOM_KEYBOARD, true);
            let touchpad_atom = state.atoms.intern(crate::xinput::XI_ATOM_TOUCHPAD, true);
            let listed_devices = state
                .xi_devices
                .query(0)
                .expect("XIAllDevices selector is always valid");
            // Xorg ListInputDevices copies the live ButtonClass and
            // ValuatorClass counts (`Xi/listdev.c:100-107, 143-155,
            // 277-283`). Build each class vector from the same registry shape
            // used by XIQueryDevice; the axis metadata remains shape-specific.
            let class_storage: Vec<Vec<x11::Xi1DeviceClass<'_>>> = listed_devices
                .iter()
                .map(|device| {
                    let shape = device.class_shape;
                    let mut classes = Vec::with_capacity(3);
                    let button_count = shape.button_count();
                    if button_count != 0 {
                        classes.push(x11::Xi1DeviceClass::Button {
                            num_buttons: u16::from(button_count),
                        });
                    }
                    let valuator_count = usize::from(shape.valuator_count());
                    if valuator_count != 0 {
                        let shape_axes: &[(i32, i32)] = match shape {
                            crate::xinput::XiClassShape::CorePointer => &CORE_POINTER_AXES,
                            crate::xinput::XiClassShape::PhysicalPointer => &PHYSICAL_POINTER_AXES,
                            crate::xinput::XiClassShape::Keyboard => &[],
                        };
                        classes.push(x11::Xi1DeviceClass::Valuator {
                            mode: 0,
                            axes: &shape_axes[..valuator_count],
                        });
                    }
                    if shape == crate::xinput::XiClassShape::Keyboard {
                        classes.push(x11::Xi1DeviceClass::Key {
                            min_keycode: XI1_KEY_MIN,
                            max_keycode: XI1_KEY_MAX,
                            num_keys: 248,
                        });
                    }
                    classes
                })
                .collect();
            let devices = listed_devices
                .iter()
                .zip(&class_storage)
                .map(|(device, classes)| {
                    let (use_code, type_atom, classes) = match device.id {
                        crate::xinput::DEVICEID_MASTER_POINTER => (0, no_type_atom, &classes[..]),
                        crate::xinput::DEVICEID_MASTER_KEYBOARD => (1, no_type_atom, &classes[..]),
                        crate::xinput::DEVICEID_XTEST_POINTER => (4, no_type_atom, &classes[..]),
                        crate::xinput::DEVICEID_XTEST_KEYBOARD => (3, no_type_atom, &classes[..]),
                        _ => match device
                            .facet
                            .expect("registered physical XI device has a facet")
                        {
                            crate::xinput::XiFacetKind::PointerTouch => (
                                4,
                                if device.is_touchpad {
                                    touchpad_atom
                                } else {
                                    mouse_atom
                                },
                                &classes[..],
                            ),
                            crate::xinput::XiFacetKind::Keyboard => {
                                (3, keyboard_atom, &classes[..])
                            }
                        },
                    };
                    let attachment = match device.id {
                        crate::xinput::DEVICEID_MASTER_POINTER => {
                            crate::xinput::DEVICEID_MASTER_KEYBOARD
                        }
                        crate::xinput::DEVICEID_MASTER_KEYBOARD => {
                            crate::xinput::DEVICEID_MASTER_POINTER
                        }
                        _ => device.attached_master.unwrap_or(0),
                    };
                    x11::Xi1DeviceDescriptor {
                        id: device.id,
                        use_code,
                        attachment,
                        type_atom,
                        name: &device.name,
                        classes,
                    }
                })
                .collect::<Vec<_>>();
            buf.extend_from_slice(&x11::encode_list_input_devices_reply(
                byte_order, sequence, &devices,
            ));
        }
        // XI 1.x SelectExtensionEvent (minor 6; xSelectExtensionEventReq,
        // XIproto.h). Body after the 4-byte generic header:
        //   body[0..4] window (Window = CARD32),
        //   body[4..6] count  (CARD16, number of XEventClass values),
        //   body[6..8] pad00  (CARD16),
        //   body[8..]  count × XEventClass (CARD32).
        //
        // An XEventClass packs `(deviceid << 8) | event_code`. We only
        // implement XI1 `DevicePropertyNotify` (event code 16 within
        // the XInput block, i.e. low byte = `XI_FIRST_EVENT + 16` = 82
        // — yserver assigns XInput the contiguous block 66..=82), so
        // classes for other XI1 events (motion / button / key / etc.)
        // are accepted but ignored. We do NOT error on unknown classes:
        // a single SelectExtensionEvent often selects multiple classes
        // and Xorg silently drops the ones it can't service.
        //
        // Selection state is canonical per window, as in Xorg. Events such
        // as DevicePropertyNotify carry no event-window field, so a separate
        // aggregate is derived from the per-window state for delivery.
        //
        // Replace semantics: for every deviceid mentioned in the
        // supplied class list, this client's prior selections for that
        // deviceid on this window are dropped before the new set is
        // installed. A client
        // unsubscribes a given deviceid by passing at least one class
        // for it whose low byte is not `DevicePropertyNotify` — the
        // deviceid lands in `touched_devices` and its prior entries
        // are cleared; the unsupported class itself is silently
        // dropped. `count == 0` is a no-op (the existing selection
        // survives unchanged), matching Xorg's behaviour.
        6 => {
            if body.len() < 8 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    16, // BadLength
                    0,
                    6,
                    XI2_MAJOR_OPCODE,
                );
            }
            let count = usize::from(u16::from_le_bytes([body[4], body[5]]));
            let expected_len = 8usize.saturating_add(count.saturating_mul(4));
            if body.len() < expected_len {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    16, // BadLength
                    0,
                    6,
                    XI2_MAJOR_OPCODE,
                );
            }
            // Xorg Xi/selectev.c validates the window first (BadWindow),
            // then rejects classes whose embedded deviceid names no
            // device (BadClass) — XTS XSelectExtensionEvent-11/-13.
            let sel_window = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if state.resources.window(ResourceId(sel_window)).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    sel_window,
                    u16::from(minor),
                    XI2_MAJOR_OPCODE,
                );
            }
            // Xorg's HandleDevicePresenceMask runs before ordinary XI1
            // class validation and removes every class whose device field
            // is 256. Only its `_devicePresence` class selects a mask.
            // Preserve that class separately: truncating the device field
            // to eight bits would turn it into XIAllDevices (id 0).
            let mut ordinary_classes = Vec::with_capacity(count);
            let mut presence_selected = false;
            for i in 0..count {
                let off = 8 + i * 4;
                let class =
                    u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
                if class >> 8 == 256 {
                    if class & 0xff == 0 {
                        presence_selected = true;
                    }
                    continue;
                }
                ordinary_classes.push(class);
            }
            if presence_selected && let Some(client) = state.clients.get_mut(&client_id.0) {
                // Xorg installs DevicePresenceNotifyMask in
                // HandleDevicePresenceMask before ordinary classes are
                // validated. Preserve that ordering if a later class in
                // this request reports BadClass.
                client
                    .xi1_window_event_classes
                    .entry(ResourceId(sel_window))
                    .or_default()
                    .insert(crate::xinput::XI1_DEVICE_PRESENCE_CLASS);
                client.rebuild_xi1_global_event_classes();
            }
            for class in &ordinary_classes {
                if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(*class)) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_CLASS,
                        *class,
                        u16::from(minor),
                        XI2_MAJOR_OPCODE,
                    );
                }
            }
            // Decode the requested class list and collect the set of
            // deviceids that appear (so we can drop the client's stale
            // entries for *those* devices — Xorg's "replace per device"
            // semantics, see Xi/selectev.c::ProcXSelectExtensionEvent).
            let mut accepted_classes: Vec<u32> = Vec::with_capacity(ordinary_classes.len());
            let mut touched_devices: HashSet<u8> = HashSet::new();
            for class in ordinary_classes {
                #[allow(clippy::cast_possible_truncation)]
                let class_low = class as u8; // event code in the XInput block
                #[allow(clippy::cast_possible_truncation)]
                let dev_byte = (class >> 8) as u8;
                touched_devices.insert(dev_byte);
                // DevicePropertyNotify classes are device-scoped; the
                // input-event classes (DeviceKeyPress..DeviceMotionNotify),
                // the focus classes (DeviceFocusIn/Out, consumed by
                // `xi1_focus::emit_device_focus`) and DeviceStateNotify
                // (consumed by `xi1_state_notify::deliver_state_notify`)
                // select per window like core input events and are
                // recorded in `xi1_window_event_classes` below. Other
                // classes are silently discarded, matching Xorg: it walks
                // `xi_all_events` and skips entries it cannot service.
                if crate::server::xi1_class_is_global_notification(class) {
                    // DevicePropertyNotify / DeviceMappingNotify /
                    // ChangeDeviceNotify are server-wide per-device
                    // events: Xorg `Xi/exevents.c::SendMappingNotify` /
                    // `SendDeviceNotify` fan them out by walking the
                    // global client list, NOT a per-window event-mask.
                    accepted_classes.push(class);
                } else if (XI_FIRST_EVENT + XI_DEVICE_KEY_PRESS_OFFSET
                    ..=XI_FIRST_EVENT + crate::xinput::XI_DEVICE_FOCUS_OUT_OFFSET)
                    .contains(&class_low)
                    || class_low == XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET
                {
                    accepted_classes.push(class);
                }
            }
            debug!(
                "client {} #{} XSelectExtensionEvent window=0x{:x} count={} accepted={} window_input={}",
                client_id.0,
                sequence.0,
                sel_window,
                count,
                accepted_classes
                    .iter()
                    .filter(|class| crate::server::xi1_class_is_global_notification(**class))
                    .count(),
                accepted_classes
                    .iter()
                    .filter(|class| !crate::server::xi1_class_is_global_notification(**class))
                    .count(),
            );
            if let Some(client) = state.clients.get_mut(&client_id.0) {
                // Selection state belongs to the request window. Drop stale
                // entries for the named devices on that window only (Xorg
                // replacement semantics), then install the accepted list.
                if !touched_devices.is_empty()
                    && let Some(set) = client
                        .xi1_window_event_classes
                        .get_mut(&ResourceId(sel_window))
                {
                    set.retain(|c| {
                        if *c == crate::xinput::XI1_DEVICE_PRESENCE_CLASS {
                            return true;
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        let dev_byte = (c >> 8) as u8;
                        !touched_devices.contains(&dev_byte)
                    });
                }
                if !accepted_classes.is_empty() {
                    let set = client
                        .xi1_window_event_classes
                        .entry(ResourceId(sel_window))
                        .or_default();
                    for class in accepted_classes {
                        set.insert(class);
                    }
                }

                // Keep the delivery-oriented aggregate in sync. Global XI1
                // notifications have no event-window field, but selecting
                // them is still per-window protocol state.
                client.rebuild_xi1_global_event_classes();
            }
            return Ok(RequestOutcome::Handled);
        }
        // XI 1.x ListDeviceProperties (minor 36; xListDevicePropertiesReq,
        // XIproto.h:1433-1440). Body after the 4-byte generic header:
        //   body[0] deviceid (CARD8), body[1] pad0, body[2..4] pad1.
        // MATE's settings daemon enumerates a touchpad's libinput
        // properties through this XI1 path (not the XI2 XIListProperties),
        // so a real reply backed by `xi_devices` is required.
        36 => {
            if body.is_empty() {
                return Ok(RequestOutcome::Handled);
            }
            let deviceid = u16::from(body[0]);
            debug!(
                "client {} #{} XListDeviceProperties deviceid={}",
                client_id.0, sequence.0, deviceid
            );
            let Some(device) = crate::xinput::find_device(&state.xi_devices, deviceid) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    36,
                    XI2_MAJOR_OPCODE,
                );
            };
            buf.extend_from_slice(&crate::xinput::encode_xi1_list_properties_reply(
                byte_order, sequence, device,
            ));
        }
        // XI 1.x ChangeDeviceProperty (minor 37; xChangeDevicePropertyReq,
        // XIproto.h:1462-1473). Body after the 4-byte generic header:
        //   body[0..4] property (Atom), body[4..8] type (Atom),
        //   body[8] deviceid (CARD8), body[9] format, body[10] mode,
        //   body[11] pad, body[12..16] nUnits (CARD32), body[16..] value.
        37 => {
            if body.len() < 16 {
                return Ok(RequestOutcome::Handled);
            }
            let property = AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let type_atom = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let deviceid = u16::from(body[8]);
            let format = body[9];
            let mode = body[10];
            let num_items = u32::from_le_bytes([body[12], body[13], body[14], body[15]]) as usize;
            debug!(
                "client {} #{} XChangeDeviceProperty deviceid={} mode={} format={} property={} type={} num_items={}",
                client_id.0, sequence.0, deviceid, mode, format, property.0, type_atom.0, num_items
            );
            // Device lookup first, mirroring the XI2 path (xserver does
            // dixLookupDevice before the mode/format checks).
            if crate::xinput::find_device(&state.xi_devices, deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    37,
                    XI2_MAJOR_OPCODE,
                );
            }
            if mode != crate::xinput::XI_PROP_MODE_REPLACE
                && mode != crate::xinput::XI_PROP_MODE_PREPEND
                && mode != crate::xinput::XI_PROP_MODE_APPEND
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    2, // BadValue
                    u32::from(mode),
                    37,
                    XI2_MAJOR_OPCODE,
                );
            }
            if format != 8 && format != 16 && format != 32 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    2, // BadValue
                    u32::from(format),
                    37,
                    XI2_MAJOR_OPCODE,
                );
            }
            let value_len = num_items.saturating_mul(usize::from(format / 8));
            let Some(data) = body.get(16..16 + value_len) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    16, // BadLength
                    0,
                    37,
                    XI2_MAJOR_OPCODE,
                );
            };
            let data = canonicalize_xi_property_data(byte_order, format, data);
            // The T3 spec §D pipeline is shared with the XI2 arm
            // (minor 57) via `dispatch_change_property`; the same
            // `emit_property_change` helper fans out
            // `XI_PropertyEvent` here too — xserver's
            // `send_property_event` fires from both XI1 and XI2 paths.
            match dispatch_change_property(
                state, backend, client_id, sequence, 37, deviceid, mode, format, property,
                type_atom, &data,
            ) {
                Ok(PropertyChangeOutcome::Changed(what)) => {
                    let _ = emit_property_change(state, deviceid, property, what);
                    return Ok(RequestOutcome::Handled);
                }
                Ok(PropertyChangeOutcome::Pending(request)) => {
                    return Ok(RequestOutcome::PendingXiConfig(request));
                }
                Err(err) => {
                    return emit_property_dispatch_error(state, client_id, sequence, err, 37);
                }
            }
        }
        // XI 1.x DeleteDeviceProperty (minor 38; xDeleteDevicePropertyReq,
        // XIproto.h:1481-1489). Body after the 4-byte generic header:
        //   body[0..4] property (Atom), body[4] deviceid (CARD8),
        //   body[5] pad0, body[6..8] pad1.
        38 => {
            if body.len() < 5 {
                return Ok(RequestOutcome::Handled);
            }
            let property = AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let deviceid = u16::from(body[4]);
            debug!(
                "client {} #{} XDeleteDeviceProperty deviceid={} property={}",
                client_id.0, sequence.0, deviceid, property.0
            );
            // T3: BadAtom guard (mirror of XI2 minor 58).
            if !state.atoms.exists(property) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    5, // BadAtom
                    property.0,
                    38,
                    XI2_MAJOR_OPCODE,
                );
            }
            if crate::xinput::find_device(&state.xi_devices, deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    38,
                    XI2_MAJOR_OPCODE,
                );
            }
            if is_xtest_marker_property(state, deviceid, property) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    10, // BadAccess: XTEST Device is protected by atom identity.
                    property.0,
                    38,
                    XI2_MAJOR_OPCODE,
                );
            }
            if crate::xinput::find_device(&state.xi_devices, deviceid)
                .and_then(|device| device.properties.get(&property))
                .is_some_and(|property| !property.deletable)
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    10, // BadAccess: seeded driver properties are non-deletable.
                    property.0,
                    38,
                    XI2_MAJOR_OPCODE,
                );
            }
            let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid)
                .expect("device existence verified above");
            if let Some(what) = crate::xinput::apply_delete_property(device, property) {
                // Same XI2 fan-out the minor-58 arm runs.
                let _ = emit_property_change(state, deviceid, property, what);
            }
            return Ok(RequestOutcome::Handled);
        }
        // XI 1.x GetDeviceProperty (minor 39; xGetDevicePropertyReq,
        // XIproto.h:1497-1512). Body after the 4-byte generic header:
        //   body[0..4] property (Atom), body[4..8] type (Atom),
        //   body[8..12] longOffset (CARD32), body[12..16] longLength
        //   (CARD32), body[16] deviceid (CARD8), body[17] delete (BOOL),
        //   body[18..20] pad.
        // This is the request MATE's settings daemon uses to read
        // "libinput Tapping Enabled" etc. The reply echoes the requested
        // deviceid (byte 21), unlike the XI2 reply (Task brief: the old
        // zero-stub returned device=0 and MATE rejected the touchpad).
        39 => {
            if body.len() < 18 {
                return Ok(RequestOutcome::Handled);
            }
            let property = AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
            let req_type = AtomId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
            let offset = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            let len = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
            let deviceid = u16::from(body[16]);
            let delete_raw = body[17];
            debug!(
                "client {} #{} XGetDeviceProperty deviceid={} property={} type={} offset={} len={} delete={}",
                client_id.0, sequence.0, deviceid, property.0, req_type.0, offset, len, delete_raw
            );
            // T3: BadAtom guard (mirror of XI2 minor 59).
            if !state.atoms.exists(property) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    5, // BadAtom
                    property.0,
                    39,
                    XI2_MAJOR_OPCODE,
                );
            }
            if crate::xinput::find_device(&state.xi_devices, deviceid).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    XI2_FIRST_ERROR, // XI_BadDevice
                    u32::from(deviceid),
                    39,
                    XI2_MAJOR_OPCODE,
                );
            }
            if delete_raw > 1 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    2, // BadValue
                    u32::from(delete_raw),
                    39,
                    XI2_MAJOR_OPCODE,
                );
            }
            let delete = delete_raw != 0;
            let device = crate::xinput::find_device_mut(&mut state.xi_devices, deviceid)
                .expect("device existence verified above");
            let send_deleted = match crate::xinput::encode_xi1_get_property_reply(
                byte_order, sequence, device, property, req_type, offset, len, delete,
            ) {
                Ok(reply) => {
                    let bytes_after = x11::read_u32(byte_order, &reply[12..16]);
                    buf.extend_from_slice(&reply);
                    delete && bytes_after == 0
                }
                Err(_) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        2, // BadValue
                        offset,
                        39,
                        XI2_MAJOR_OPCODE,
                    );
                }
            };
            if send_deleted {
                let _ = emit_property_change(
                    state,
                    deviceid,
                    property,
                    crate::xinput::PropWhat::Deleted,
                );
            }
        }
        // XI1 OpenDevice (minor 3). Xlib's `DevicePropertyNotify(dev, type,
        // _class)` macro walks `XDevice->classes` (populated from this
        // reply) for an `OtherClass(6)` entry, then computes
        //   type  = entry.event_type_base + _propertyNotify(=6)
        //   _class = (deviceid << 8) | type
        // and passes _class to `XSelectExtensionEvent`. An empty class
        // list (our pre-fix stub) leaves _class = 0, and `xinput
        // watch-props` silently subscribes to nothing.
        //
        // Verified against `mate-asahi-xorg.xtrace`: pointer devices return
        // Button(1)/Valuator(2)/Feedback(3)/Other(6), while keyboards return
        // Key(0)/Feedback(3)/Focus(5)/Other(6). The Other entry uses
        // `event_type_base = 0x4c = 76`, and xinput's subsequent
        // `SelectExtensionEvent` carries class `(deviceid<<8)|82`.
        3 => {
            // Xorg Xi/opendev.c: unknown IDs and master devices yield
            // BadDevice. XOpenDevice opens the listed slave devices,
            // including each live physical facet.
            {
                let deviceid = u16::from(*body.first().unwrap_or(&0));
                if !xi1_device_valid(&state.xi_devices, deviceid)
                    || xi1_device_is_master(&state.xi_devices, deviceid)
                {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_DEVICE,
                        u32::from(deviceid),
                        u16::from(minor),
                        XI2_MAJOR_OPCODE,
                    );
                }
            }
            // Mirror Xorg's OpenDevice reply layout (verified in
            // mate-asahi-xorg.xtrace line 142667): four XInputClassInfo
            // entries in the order Button(1), Valuator(2), Feedback(3),
            // Other(6) with `event_type_base` values that line up the
            // XI1 wire event codes against `XI_FIRST_EVENT`.
            //
            // The Other entry is the load-bearing one for property
            // notification: xinput's Xlib `DevicePropertyNotify(dev,
            // type, _class)` macro walks `XDevice->classes`, finds the
            // OtherClass entry, and computes
            //   type   = entry.event_type_base + _propertyNotify(=6)
            //   _class = (deviceid << 8) | type
            // and passes _class to `XSelectExtensionEvent`. An empty
            // class list (our pre-fix stub) leaves _class = 0 and
            // `xinput watch-props` silently subscribes to nothing.
            //
            // The Button/Valuator/Feedback entries carry only their
            // (class, event_type_base) tags here — the OpenDevice
            // reply itself never includes per-class trailing data
            // (buttons / axes / feedback descriptors live in separate
            // requests). Including them empty-shaped matches Xorg's
            // wire byte-for-byte, which Xlib appears to need for the
            // `XDevice` to register as fully usable even though only
            // the OtherClass entry is consumed for property events.
            // Class tags (XI.h) + event_type_base offsets relative to
            // `XI_FIRST_EVENT` (= 66; Xorg `Xi/extinit.c::FixExtensionEvents`):
            //   Key(0)      base + 1  = 0x43   Button(1)   base + 3  = 0x45
            //   Valuator(2) base + 5  = 0x47   Feedback(3) base 0 (no event)
            //   Focus(5)    base + 6  = 0x48   Other(6)    base + 10 = 0x4c
            const KEY_CLASS: u8 = 0;
            const BUTTON_CLASS: u8 = 1;
            const VALUATOR_CLASS: u8 = 2;
            const FEEDBACK_CLASS: u8 = 3;
            const FOCUS_CLASS: u8 = 5;
            const OTHER_CLASS: u8 = 6;
            // The class set MUST match the device's use: a pointer reports
            // Button/Valuator/Feedback/Other; a keyboard reports
            // Key/Feedback/Focus/Other (verified against mate-xorg.xtrace
            // OpenDevice replies). Returning pointer classes for a keyboard
            // contradicts ListInputDevices and trips clients' device model.
            let deviceid = u16::from(*body.first().unwrap_or(&0));
            let is_keyboard = xi1_device_has_keys(&state.xi_devices, deviceid);
            let entries: [u8; 8] = if is_keyboard {
                [
                    KEY_CLASS,
                    XI_FIRST_EVENT + 1,
                    FEEDBACK_CLASS,
                    0,
                    FOCUS_CLASS,
                    XI_FIRST_EVENT + 6,
                    OTHER_CLASS,
                    XI_FIRST_EVENT + 10,
                ]
            } else {
                [
                    BUTTON_CLASS,
                    XI_FIRST_EVENT + 3,
                    VALUATOR_CLASS,
                    XI_FIRST_EVENT + 5,
                    FEEDBACK_CLASS,
                    0,
                    OTHER_CLASS,
                    XI_FIRST_EVENT + 10,
                ]
            };
            // CRITICAL: `num_classes` lives at byte 8 of xOpenDeviceReply
            // (the first field after the 8-byte header), NOT the detail
            // byte. Clients (Xlib XOpenDevice, Chromium's XI device init)
            // read it there; the old code passed it as `fixed_reply`'s
            // detail byte, leaving byte 8 = 0, so num_classes read as 0 on
            // the wire and the class array below was invisible. That made
            // OpenDevice contradict ListInputDevices' per-device class
            // count → Chromium fatal CHECK / SIGTRAP on startup.
            // length=2 = two extra 4-byte units (8 bytes of class entries).
            let mut reply = x11::fixed_reply(byte_order, sequence, 3, 2);
            reply.push(4); // byte 8: num_classes
            reply.extend_from_slice(&[0u8; 23]); // bytes 9..=31: header pad
            reply.extend_from_slice(&entries);
            debug!(
                "client {} #{} XI1 OpenDevice device={deviceid} -> {} classes",
                client_id.0, sequence.0, 4
            );
            buf.extend_from_slice(&reply);
        }
        // ── XI 1.x minors with request validation ──────────────────
        //
        // Spec-error checks first (the XTS XI scenario's "Got Success,
        // Expecting <error>" family — BadDevice/BadValue/BadMatch/
        // BadWindow/BadMode/BadClass per the XInput 1.x protocol spec,
        // cross-checked against Xorg Xi/*.c). Success behavior is described
        // per request below; capability boundaries such as absent motion
        // history and device-resolution ranges are explicit rather than a
        // blanket zero-reply fallback.

        // CloseDevice (void): xCloseDeviceReq { deviceid }.
        4 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            debug!(
                "client {} #{} XI1 CloseDevice device={dev}",
                client_id.0, sequence.0
            );
        }
        // SetDeviceMode: { deviceid, mode }. Mode changes only make
        // sense for devices with valuators (Xorg Xi/setmode.c).
        5 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_valuators(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // Store the mode (Relative=0 / Absolute=1) —
            // DeviceStateNotify reports it in the `classes_reported`
            // bits above ModeBitsShift. Xorg
            // `Xi/setmode.c::ProcXSetDeviceMode` returns
            // `AlreadyGrabbed` (reply byte 8) when another client
            // holds the device's active grab; in that case the mode
            // is NOT updated. xts5 XSetDeviceMode-2.
            const ALREADY_GRABBED: u8 = 1;
            let grabbed_elsewhere = state
                .xi1_active_grabs
                .get(&dev)
                .is_some_and(|g| g.owner != client_id);
            let status = if grabbed_elsewhere {
                ALREADY_GRABBED
            } else {
                let mode = *body.get(1).unwrap_or(&0);
                if mode <= 1 {
                    state
                        .xi1_device_input_state
                        .entry(dev)
                        .or_default()
                        .valuator_mode = mode;
                }
                0
            };
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            reply.push(status);
            reply.extend_from_slice(&[0u8; 23]);
            buf.extend_from_slice(&reply);
        }
        // GetSelectedExtensionEvents: { window }. The trailing payload is
        // this client's classes followed by all clients' classes for the
        // requested window (Xorg Xi/getselev.c).
        7 => {
            let win = u32::from_le_bytes([
                *body.first().unwrap_or(&0),
                *body.get(1).unwrap_or(&0),
                *body.get(2).unwrap_or(&0),
                *body.get(3).unwrap_or(&0),
            ]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let window = ResourceId(win);
            let mut this_client: Vec<u32> = state
                .clients
                .get(&client_id.0)
                .and_then(|client| client.xi1_window_event_classes.get(&window))
                .map(|classes| classes.iter().copied().collect())
                .unwrap_or_default();
            this_client.sort_unstable();

            let mut all_clients: Vec<u32> = state
                .clients
                .values()
                .filter_map(|client| client.xi1_window_event_classes.get(&window))
                .flat_map(|classes| classes.iter().copied())
                .collect();
            all_clients.sort_unstable();

            let total_classes = this_client.len().saturating_add(all_clients.len());
            let length_words = u32::try_from(total_classes).unwrap_or(u32::MAX);
            let this_count = u16::try_from(this_client.len()).unwrap_or(u16::MAX);
            let all_count = u16::try_from(all_clients.len()).unwrap_or(u16::MAX);
            let mut reply = x11::fixed_reply(byte_order, sequence, minor, length_words);
            x11::write_u16(byte_order, &mut reply, this_count);
            x11::write_u16(byte_order, &mut reply, all_count);
            reply.extend_from_slice(&[0u8; 20]);
            for class in this_client.into_iter().chain(all_clients) {
                x11::write_u32(byte_order, &mut reply, class);
            }
            buf.extend_from_slice(&reply);
        }
        // ChangeDeviceDontPropagateList (void): { window, count, mode },
        // classes follow. mode ∈ {AddToList=0, DeleteFromList=1}.
        8 => {
            if body.len() < 8 {
                return Ok(RequestOutcome::Handled);
            }
            let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let mode = body[6];
            if mode > 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_MODE,
                    u32::from(mode),
                    minor,
                );
            }
            let count = usize::from(u16::from_le_bytes([body[4], body[5]]));
            let mut classes: Vec<u32> = Vec::with_capacity(count);
            for i in 0..count {
                let off = 8 + i * 4;
                if off + 4 > body.len() {
                    break;
                }
                let class =
                    u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
                if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_CLASS,
                        class,
                        minor,
                    );
                }
                classes.push(class);
            }
            // mode 0 = AddToList, 1 = DeleteFromList. Xorg
            // `Xi/chgdprop.c::ChangeDeviceDontPropagateList` applies the
            // change to `OtherInputMasks.dontPropagateMask[deviceid]`;
            // the deviceid is encoded in the high byte of each class so
            // the per-device split is implicit in the class set.
            let entry = state
                .xi1_window_dont_propagate
                .entry(ResourceId(win))
                .or_default();
            if mode == 0 {
                for c in &classes {
                    entry.insert(*c);
                }
            } else {
                for c in &classes {
                    entry.remove(c);
                }
                if entry.is_empty() {
                    state.xi1_window_dont_propagate.remove(&ResourceId(win));
                }
            }
            debug!(
                "client {} #{} XI1 ChangeDeviceDontPropagateList win=0x{win:x} mode={mode} count={count}",
                client_id.0, sequence.0
            );
        }
        // GetDeviceDontPropagateList: { window }. Reply layout
        // (XIproto.h:`xGetDeviceDontPropagateListReply`):
        //   bytes 0..1 = reply opcode (1) + pad
        //   bytes 2..4 = sequence
        //   bytes 4..8 = length (32-bit units of trailing class array)
        //   bytes 8..10 = count (CARD16)
        //   bytes 10..32 = pad
        //   trailing `count` × XEventClass (CARD32) at byte 32
        9 => {
            let win = u32::from_le_bytes([
                *body.first().unwrap_or(&0),
                *body.get(1).unwrap_or(&0),
                *body.get(2).unwrap_or(&0),
                *body.get(3).unwrap_or(&0),
            ]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let classes: Vec<u32> = state
                .xi1_window_dont_propagate
                .get(&ResourceId(win))
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default();
            #[allow(clippy::cast_possible_truncation)]
            let count = classes.len() as u16;
            let length_words = u32::try_from(classes.len()).unwrap_or(u32::MAX);
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, length_words);
            x11::write_u16(byte_order, &mut reply, count); // bytes 8..10
            reply.extend_from_slice(&[0u8; 22]); // pad to 32
            for c in &classes {
                x11::write_u32(byte_order, &mut reply, *c);
            }
            buf.extend_from_slice(&reply);
        }
        // GetDeviceMotionEvents: { start, stop, deviceid }. Motion
        // history needs valuators; the reply axis count comes from the live
        // class (`Xi/gtmotion.c:107-120`).
        10 => {
            let dev = u16::from(*body.get(8).unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_valuators(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            let start = u32::from_le_bytes(body[0..4].try_into().expect("four bytes"));
            let stop = u32::from_le_bytes(body[4..8].try_into().expect("four bytes"));
            let history = motion_history_range(state, start, stop);
            const XI_ABSOLUTE: u8 = 1;
            let axis_count = xi1_device_valuator_count(&state.xi_devices, dev)
                .expect("validated valuator class");
            let words_per_event = 1 + u32::from(axis_count);
            let length_words = u32::try_from(history.len())
                .unwrap_or(u32::MAX)
                .saturating_mul(words_per_event);
            let mut reply = x11::fixed_reply(byte_order, sequence, minor, length_words);
            x11::write_u32(
                byte_order,
                &mut reply,
                u32::try_from(history.len()).unwrap_or(u32::MAX),
            );
            reply.push(axis_count);
            reply.push(XI_ABSOLUTE);
            reply.extend_from_slice(&[0u8; 18]);
            for record in history {
                x11::write_u32(byte_order, &mut reply, record.time);
                x11::write_u32(
                    byte_order,
                    &mut reply,
                    i32::from(record.root_x).cast_unsigned(),
                );
                x11::write_u32(
                    byte_order,
                    &mut reply,
                    i32::from(record.root_y).cast_unsigned(),
                );
                for _ in 2..axis_count {
                    x11::write_u32(byte_order, &mut reply, 0);
                }
            }
            buf.extend_from_slice(&reply);
        }
        // ChangeKeyboardDevice: { deviceid }. Needs a device with keys
        // (Xorg Xi/chgkbd.c). xts5 expects:
        //   1. ChangeDeviceNotify (request=1 = NewKeyboard)
        //   2. core MappingNotify (request=1 = MappingKeyboard)
        //   3. reply
        // — in that order on the wire.
        11 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_keys(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // 1. ChangeDeviceNotify for originator (if selected) + fanout.
            if crate::core_loop::xi1_focus::xi1_client_wants_change_device_notify(
                state, client_id, dev,
            ) {
                let time = state.timestamp_now();
                #[allow(clippy::cast_possible_truncation)]
                let device_byte = dev as u8;
                crate::xinput::encode_xi1_change_device_notify(
                    &mut buf,
                    byte_order,
                    crate::server::XI_FIRST_EVENT + crate::xinput::XI_CHANGE_DEVICE_NOTIFY_OFFSET,
                    device_byte,
                    sequence,
                    time,
                    1, // NewKeyboard
                );
            }
            crate::core_loop::xi1_focus::emit_change_device_notify(state, client_id, dev, 1);
            // 2. core MappingNotify (request=1 = MappingKeyboard) to all
            // clients, including originator inline so the wire order is
            // ChangeDeviceNotify → MappingNotify → reply.
            let _ = x11::write_mapping_notify_event(&mut buf, byte_order, sequence, 1, 0, 0);
            let others: Vec<ClientId> = state
                .clients
                .keys()
                .filter(|id| **id != client_id.0)
                .map(|id| ClientId(*id))
                .collect();
            let _dropped =
                crate::core_loop::fanout::fanout_event_to_clients(state, &others, |b, s, o| {
                    let _ = x11::write_mapping_notify_event(b, o, s, 1, 0, 0);
                });
            // 3. reply.
            buf.extend_from_slice(&xi1_zero_reply(byte_order, sequence));
        }
        // ChangePointerDevice is unsupported by Xorg 21.1.24: its handler
        // returns BadDevice after request-size validation (Xi/chgptr.c:92-98).
        12 => {
            return xi1_error(state, client_id, sequence, XI1_ERROR_BAD_DEVICE, 0, minor);
        }
        // GrabDevice: { grabWindow, time, event_count, this_device_mode,
        // other_devices_mode, ownerEvents, deviceid } + classes. Classes
        // must name the grabbed device (XTS XGrabDevice-4/-13).
        13 => {
            if body.len() < 16 {
                return Ok(RequestOutcome::Handled);
            }
            let dev = u16::from(body[13]);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let this_mode = body[10];
            let other_mode = body[11];
            let owner_events = body[12];
            if this_mode > 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(this_mode),
                    minor,
                );
            }
            if other_mode > 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(other_mode),
                    minor,
                );
            }
            if owner_events > 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(owner_events),
                    minor,
                );
            }
            let count = usize::from(u16::from_le_bytes([body[8], body[9]]));
            for i in 0..count {
                let off = 16 + i * 4;
                if off + 4 > body.len() {
                    break;
                }
                let class =
                    u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
                let class_dev = xi1_event_class_device(class);
                if !xi1_device_valid(&state.xi_devices, class_dev) || class_dev != dev {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_CLASS,
                        class,
                        minor,
                    );
                }
            }
            // Establish the active device grab. Status values per X.h:
            // GrabSuccess 0, AlreadyGrabbed 1, GrabInvalidTime 2,
            // GrabNotViewable 3.
            let time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let now = state
                .timestamp_now()
                .max(state.xi1_last_input_time)
                .max(state.xi1_last_grab_time);
            let viewable = state
                .resources
                .window(ResourceId(win))
                .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
            let status: u8 = if state
                .xi1_active_grabs
                .get(&dev)
                .is_some_and(|g| g.owner != client_id)
            {
                1 // AlreadyGrabbed
            } else if !viewable {
                3 // GrabNotViewable
            } else if time != 0 && (time < state.xi1_last_grab_time || time > now) {
                2 // GrabInvalidTime
            } else {
                state.xi1_active_grabs.insert(
                    dev,
                    crate::server::Xi1ActiveGrab {
                        owner: client_id,
                        deviceid: dev,
                        grab_window: ResourceId(win),
                        owner_events: owner_events != 0,
                        this_mode,
                        other_mode,
                        passive_detail: None,
                    },
                );
                state.xi1_last_grab_time = if time == 0 { now } else { time };
                // CheckGrabForSyncs: a sync this_mode freezes the
                // device ONCE (FrozenNoEvent — no stored event, so
                // Replay is a no-op on it); a sync other_mode holds
                // the paired device on this grab's behalf.
                crate::core_loop::pointer_fanout::xi1_check_grab_for_syncs(
                    state,
                    dev,
                    client_id,
                    this_mode == 0,
                    other_mode == 0,
                );
                0 // GrabSuccess
            };
            debug!(
                "client {} #{} XI1 GrabDevice device={dev} window=0x{win:x} status={status}",
                client_id.0, sequence.0
            );
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            reply.push(status);
            reply.extend_from_slice(&[0u8; 23]);
            buf.extend_from_slice(&reply);
        }
        // UngrabDevice (void): { time, deviceid }. Body: time at
        // bytes 0..4, deviceid at byte 4 — `xUngrabDeviceReq`.
        14 => {
            let time = u32::from_le_bytes([
                *body.first().unwrap_or(&0),
                *body.get(1).unwrap_or(&0),
                *body.get(2).unwrap_or(&0),
                *body.get(3).unwrap_or(&0),
            ]);
            let dev = u16::from(*body.get(4).unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            // Xorg `Xi/ungrdev.c::ProcXUngrabDevice` requires the
            // request's time to be in `[grabTime, currentTime]`
            // (CurrentTime = 0 passes both checks). Out-of-range
            // timestamps silently skip the release — xts5
            // XUngrabDevice-2 asserts this.
            let now = state.timestamp_now();
            let time_ok = time == 0 || (time >= state.xi1_last_grab_time && time <= now);
            if time_ok
                && state
                    .xi1_active_grabs
                    .get(&dev)
                    .is_some_and(|g| g.owner == client_id)
            {
                crate::core_loop::pointer_fanout::xi1_deactivate_device_grab(state, dev);
            }
            debug!(
                "client {} #{} XI1 UngrabDevice device={dev}",
                client_id.0, sequence.0
            );
        }
        // GrabDeviceKey (void): { grabWindow, event_count, modifiers,
        // modifier_device, grabbed_device, key, this_device_mode,
        // other_devices_mode, ownerEvents } + classes. (The XTS prose
        // for XGrabDeviceKey-19/-20/-21 says "BadValue", but the test
        // error traps accept only the XI extension BadDevice/BadClass
        // codes — verified empirically; Xorg's dixLookupDevice agrees.)
        15 => {
            if body.len() < 16 {
                return Ok(RequestOutcome::Handled);
            }
            let mods = u16::from_le_bytes([body[6], body[7]]);
            let mod_dev = u16::from(body[8]);
            let dev = u16::from(body[9]);
            let key = body[10];
            let this_mode = body[11];
            let other_mode = body[12];
            let owner_events = body[13];
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(mod_dev),
                    minor,
                );
            }
            if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_has_keys(&state.xi_devices, mod_dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // key: AnyKey (0) or within the advertised keycode range.
            if key != 0 && key < XI1_KEY_MIN {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(key),
                    minor,
                );
            }
            if !xi1_modifiers_valid(mods) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(mods),
                    minor,
                );
            }
            if this_mode > 1 || other_mode > 1 || owner_events > 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(this_mode.max(other_mode).max(owner_events)),
                    minor,
                );
            }
            let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let count = usize::from(u16::from_le_bytes([body[4], body[5]]));
            for i in 0..count {
                let off = 16 + i * 4;
                if off + 4 > body.len() {
                    break;
                }
                let class =
                    u32::from_le_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
                if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_CLASS,
                        class,
                        minor,
                    );
                }
            }
            // Conflicting grab by another client → BadAccess (XTS
            // XGrabDeviceKey-16).
            if state.xi1_passive_grabs.iter().any(|g| {
                g.is_key
                    && g.owner != client_id
                    && g.deviceid == dev
                    && g.grab_window == ResourceId(win)
                    && (g.detail == key || g.detail == 0 || key == 0)
                    && (g.modifiers == mods || g.modifiers == 0x8000 || mods == 0x8000)
            }) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, minor);
            }
            state.xi1_passive_grabs.retain(|g| {
                !(g.is_key
                    && g.owner == client_id
                    && g.deviceid == dev
                    && g.grab_window == ResourceId(win)
                    && g.detail == key
                    && g.modifiers == mods)
            });
            state.xi1_passive_grabs.push(crate::server::Xi1PassiveGrab {
                owner: client_id,
                deviceid: dev,
                grab_window: ResourceId(win),
                detail: key,
                modifiers: mods,
                owner_events: owner_events != 0,
                this_mode,
                other_mode,
                is_key: true,
            });
            debug!(
                "client {} #{} XI1 GrabDeviceKey device={dev}",
                client_id.0, sequence.0
            );
        }
        // UngrabDeviceKey (void): { grabWindow, modifiers,
        // modifier_device, key, grabbed_device }.
        16 => {
            if body.len() < 9 {
                return Ok(RequestOutcome::Handled);
            }
            let mods = u16::from_le_bytes([body[4], body[5]]);
            let mod_dev = u16::from(body[6]);
            let key = body[7];
            let dev = u16::from(body[8]);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(mod_dev),
                    minor,
                );
            }
            if !xi1_device_has_keys(&state.xi_devices, dev)
                || (mod_dev != XI1_USE_X_KEYBOARD
                    && !xi1_device_has_keys(&state.xi_devices, mod_dev))
            {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            if key != 0 && key < XI1_KEY_MIN {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(key),
                    minor,
                );
            }
            if !xi1_modifiers_valid(mods) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(mods),
                    minor,
                );
            }
            let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            state.xi1_passive_grabs.retain(|g| {
                !(g.is_key
                    && g.owner == client_id
                    && g.deviceid == dev
                    && g.grab_window == ResourceId(win)
                    && (key == 0 || g.detail == key)
                    && (mods == 0x8000 || g.modifiers == mods))
            });
            debug!(
                "client {} #{} XI1 UngrabDeviceKey device={dev}",
                client_id.0, sequence.0
            );
        }
        // GrabDeviceButton (void): { grabWindow, grabbed_device,
        // modifier_device, event_count, modifiers, this_device_mode,
        // other_devices_mode, button, ownerEvents } + classes.
        17 => {
            if body.len() < 16 {
                return Ok(RequestOutcome::Handled);
            }
            let dev = u16::from(body[4]);
            let mod_dev = u16::from(body[5]);
            let mods = u16::from_le_bytes([body[8], body[9]]);
            let this_mode = body[10];
            let other_mode = body[11];
            let owner_events = body[13];
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(mod_dev),
                    minor,
                );
            }
            if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_has_keys(&state.xi_devices, mod_dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            if !xi1_modifiers_valid(mods) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(mods),
                    minor,
                );
            }
            if this_mode > 1 || other_mode > 1 || owner_events > 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(this_mode.max(other_mode).max(owner_events)),
                    minor,
                );
            }
            let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let button = body[12];
            // Validate the trailing XEventClass array (Xorg
            // `Xi/grabdevb.c::ProcXGrabDeviceButton` →
            // `CreateMaskFromList` → `BadClass`). Each class's high
            // byte names a device id; any unknown deviceid is
            // BadClass, including the `0xFFFFFFFF` sentinel xts5
            // GrabDeviceButton-21 builds. event_count lives at
            // body[6..8] per `xGrabDeviceButtonReq`.
            let event_count = usize::from(u16::from_le_bytes([body[6], body[7]]));
            for i in 0..event_count {
                let off = 16 + i * 4;
                let Some(slice) = body.get(off..off + 4) else {
                    break;
                };
                let class = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
                if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_CLASS,
                        class,
                        minor,
                    );
                }
            }
            if state.xi1_passive_grabs.iter().any(|g| {
                !g.is_key
                    && g.owner != client_id
                    && g.deviceid == dev
                    && g.grab_window == ResourceId(win)
                    && (g.detail == button || g.detail == 0 || button == 0)
                    && (g.modifiers == mods || g.modifiers == 0x8000 || mods == 0x8000)
            }) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_ACCESS, 0, minor);
            }
            state.xi1_passive_grabs.retain(|g| {
                !(!g.is_key
                    && g.owner == client_id
                    && g.deviceid == dev
                    && g.grab_window == ResourceId(win)
                    && g.detail == button
                    && g.modifiers == mods)
            });
            state.xi1_passive_grabs.push(crate::server::Xi1PassiveGrab {
                owner: client_id,
                deviceid: dev,
                grab_window: ResourceId(win),
                detail: button,
                modifiers: mods,
                owner_events: owner_events != 0,
                this_mode,
                other_mode,
                is_key: false,
            });
            debug!(
                "client {} #{} XI1 GrabDeviceButton device={dev}",
                client_id.0, sequence.0
            );
        }
        // UngrabDeviceButton (void): { grabWindow, modifiers,
        // modifier_device, button, grabbed_device }.
        18 => {
            if body.len() < 9 {
                return Ok(RequestOutcome::Handled);
            }
            let mods = u16::from_le_bytes([body[4], body[5]]);
            let mod_dev = u16::from(body[6]);
            let dev = u16::from(body[8]);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if mod_dev != XI1_USE_X_KEYBOARD && !xi1_device_valid(&state.xi_devices, mod_dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(mod_dev),
                    minor,
                );
            }
            if !xi1_device_has_buttons(&state.xi_devices, dev)
                || (mod_dev != XI1_USE_X_KEYBOARD
                    && !xi1_device_has_keys(&state.xi_devices, mod_dev))
            {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            if !xi1_modifiers_valid(mods) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(mods),
                    minor,
                );
            }
            let win = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if !xi1_window_exists(state, win) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    win,
                    minor,
                );
            }
            let button = body[7];
            state.xi1_passive_grabs.retain(|g| {
                !(!g.is_key
                    && g.owner == client_id
                    && g.deviceid == dev
                    && g.grab_window == ResourceId(win)
                    && (button == 0 || g.detail == button)
                    && (mods == 0x8000 || g.modifiers == mods))
            });
            debug!(
                "client {} #{} XI1 UngrabDeviceButton device={dev}",
                client_id.0, sequence.0
            );
        }
        // AllowDeviceEvents (void): { time, mode, deviceid }. Modes:
        // AsyncThisDevice(0)..SyncAll(5).
        19 => {
            let mode = *body.get(4).unwrap_or(&0);
            let dev = u16::from(*body.get(5).unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if mode > 5 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(mode),
                    minor,
                );
            }
            // Port of Xorg `AllowSome` (dix/events.c:1823-1936) over
            // the two-device model. Mode → newState mapping per
            // Xi/allowev.c: AsyncThisDevice(0)→THAWED,
            // SyncThisDevice(1)→FREEZE_NEXT_EVENT,
            // ReplayThisDevice(2)→NOT_GRABBED,
            // AsyncOtherDevices(3)→THAW_OTHERS,
            // AsyncAll(4)→THAWED_BOTH, SyncAll(5)→FREEZE_BOTH_NEXT_EVENT.
            use crate::{
                core_loop::pointer_fanout::{
                    xi1_compute_freezes, xi1_deactivate_device_grab, xi1_device_grab_owner,
                    xi1_other_input_device, xi1_route_device_event,
                },
                server::Xi1SyncState,
            };
            let time = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let other_dev = xi1_other_input_device(state, dev);
            let sync_state = |state: &ServerState, d: u16| {
                state
                    .xi1_frozen
                    .get(&d)
                    .map_or(Xi1SyncState::Thawed, |f| f.state)
            };
            let sync_other =
                |state: &ServerState, d: u16| state.xi1_frozen.get(&d).and_then(|f| f.other);
            // thisGrabbed / otherGrabbed / thisSynced / othersFrozen
            // (dix/events.c:1830-1851). `sync.other` stores the owning
            // client, and deactivation clears it, so `== client` means
            // "held on behalf of a live grab of this client".
            let this_grabbed = xi1_device_grab_owner(state, dev) == Some(client_id);
            let other_grabbed = other_dev
                .is_some_and(|other| xi1_device_grab_owner(state, other) == Some(client_id));
            let this_synced = sync_other(state, dev) == Some(client_id) && other_grabbed;
            let others_frozen = other_dev.is_some_and(|other| {
                other_grabbed && sync_state(state, other) >= Xi1SyncState::FrozenNoEvent
            });
            let this_frozen_by_grab =
                this_grabbed && sync_state(state, dev) >= Xi1SyncState::FrozenNoEvent;
            debug!(
                "client {} #{} XI1 AllowDeviceEvents device={dev} mode={mode} \
                 this_grabbed={this_grabbed} this_synced={this_synced} \
                 others_frozen={others_frozen}",
                client_id.0, sequence.0,
            );
            // Gate 1: only act when this device is frozen by the
            // client (directly or on another grab's behalf).
            if !(this_frozen_by_grab || this_synced) {
                return Ok(RequestOutcome::Handled);
            }
            // Gate 2: time validation — later than now or earlier than
            // the client's last grab time → no effect.
            let now = state
                .timestamp_now()
                .max(state.xi1_last_input_time)
                .max(state.xi1_last_grab_time);
            if time != 0
                && (crate::core_loop::xi1_focus::time_after(time, now)
                    || crate::core_loop::xi1_focus::time_after(state.xi1_last_grab_time, time))
            {
                return Ok(RequestOutcome::Handled);
            }
            match mode {
                // AsyncThisDevice → THAWED.
                0 => {
                    if this_grabbed && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                        f.state = Xi1SyncState::Thawed;
                    }
                    if this_synced && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                        f.other = None;
                    }
                    let xid_map = backend.xid_map().clone();
                    xi1_compute_freezes(state, backend, &xid_map);
                }
                // SyncThisDevice → FREEZE_NEXT_EVENT.
                1 => {
                    if this_grabbed {
                        state.xi1_frozen.entry(dev).or_default().state =
                            Xi1SyncState::FreezeNextEvent;
                        if this_synced && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                            f.other = None;
                        }
                        let xid_map = backend.xid_map().clone();
                        xi1_compute_freezes(state, backend, &xid_map);
                    }
                }
                // ReplayThisDevice → NOT_GRABBED: only for a grab
                // frozen WITH a stored event; release the grab and
                // reprocess that event as if the grab never took it
                // (passive grabs get a fresh look at it).
                2 => {
                    if this_grabbed && sync_state(state, dev) == Xi1SyncState::FrozenWithEvent {
                        if this_synced && let Some(f) = state.xi1_frozen.get_mut(&dev) {
                            f.other = None;
                        }
                        let stored = state.xi1_frozen.get_mut(&dev).and_then(|f| f.stored.take());
                        // Reprocessing masks out passive grabs at or
                        // above the released grab's window (Xorg
                        // syncEvents.replayWin → CheckDeviceGrabs).
                        let replay_floor = state.xi1_active_grabs.get(&dev).map(|g| g.grab_window);
                        if state.xi1_active_grabs.contains_key(&dev) {
                            xi1_deactivate_device_grab(state, dev);
                        } else if state.active_pointer_grab.is_some_and(|grab| grab.passive) {
                            // Xorg NOT_GRABBED → DeactivateGrab before the
                            // replay, passive flavor (mirrors
                            // apply_allow_events' passive NOT_GRABBED
                            // deactivation).
                            deactivate_passive_pointer_grab_crossings(state);
                        } else if state
                            .active_pointer_grab
                            .is_some_and(|g| g.owner == client_id)
                        {
                            // Explicit (or implicit) core grab: full
                            // deactivation (mirrors apply_allow_events'
                            // deactivate_core_pointer_grab call).
                            deactivate_core_pointer_grab(state, backend, client_id);
                        }
                        // Thaw dev after any deactivation. Arm A already
                        // thaws internally; re-thawing an already-thawed
                        // device is a no-op, and the no-core-grab case
                        // (previous behavior) simply thaws here.
                        if let Some(f) = state.xi1_frozen.get_mut(&dev) {
                            f.state = Xi1SyncState::Thawed;
                        }
                        if let Some(stored) = stored {
                            match stored {
                                crate::server::QueuedInputEvent::Xi1Routed(mut q) => {
                                    q.replay_floor = replay_floor;
                                    let _ = xi1_route_device_event(state, q, true);
                                }
                                crate::server::QueuedInputEvent::HostPointer(event) => {
                                    let xid_map = backend.xid_map().clone();
                                    let _ = replay_frozen_pointer_event_to_state(
                                        state, backend, &xid_map, event,
                                    );
                                }
                                crate::server::QueuedInputEvent::HostKey(event) => {
                                    let _ = replay_frozen_key_to_focus(state, event);
                                }
                                crate::server::QueuedInputEvent::HostKeyTransition(
                                    event,
                                    master_transition_accepted,
                                ) => {
                                    let _ = crate::core_loop::key_fanout::replay_frozen_key_to_focus_after_transition(
                                        state,
                                        event,
                                        master_transition_accepted,
                                    );
                                }
                                // Only a device event activates a grab, so
                                // the stored slot never holds a raw event;
                                // processing one is plain master delivery.
                                crate::server::QueuedInputEvent::RawKey(event) => {
                                    let _ = crate::core_loop::key_fanout::deliver_raw_key_master(
                                        state, event,
                                    );
                                }
                            }
                        }
                        let xid_map = backend.xid_map().clone();
                        xi1_compute_freezes(state, backend, &xid_map);
                    }
                }
                // AsyncOtherDevices → THAW_OTHERS: thaw every OTHER
                // device held by this client (this device untouched).
                3 => {
                    if others_frozen && let Some(other) = other_dev {
                        if xi1_device_grab_owner(state, other) == Some(client_id)
                            && let Some(f) = state.xi1_frozen.get_mut(&other)
                        {
                            f.state = Xi1SyncState::Thawed;
                        }
                        if sync_other(state, other) == Some(client_id)
                            && let Some(f) = state.xi1_frozen.get_mut(&other)
                        {
                            f.other = None;
                        }
                        let xid_map = backend.xid_map().clone();
                        xi1_compute_freezes(state, backend, &xid_map);
                    }
                }
                // AsyncAll → THAWED_BOTH / SyncAll → FREEZE_BOTH_NEXT_EVENT:
                // both REQUIRE the other devices to be frozen by the
                // client (XTS XAllowDeviceEvents-16/-18); they act on
                // every device the client grabbed or held.
                4 | 5 => {
                    if others_frozen {
                        let new_state = if mode == 4 {
                            Xi1SyncState::Thawed
                        } else {
                            Xi1SyncState::FreezeBothNextEvent
                        };
                        for d in std::iter::once(dev).chain(other_dev) {
                            if xi1_device_grab_owner(state, d) == Some(client_id) {
                                state.xi1_frozen.entry(d).or_default().state = new_state;
                            }
                            if sync_other(state, d) == Some(client_id)
                                && let Some(f) = state.xi1_frozen.get_mut(&d)
                            {
                                f.other = None;
                            }
                        }
                        let xid_map = backend.xid_map().clone();
                        xi1_compute_freezes(state, backend, &xid_map);
                    }
                }
                _ => unreachable!("mode validated above"),
            }
        }
        // GetDeviceFocus: { deviceid }. Reply layout
        // (xGetDeviceFocusReply, XIproto.h:709-721): focus CARD32 @8,
        // time CARD32 @12, revertTo CARD8 @16, pad to 32. The focus
        // field carries the raw stored value — None(0)/PointerRoot(1)/
        // FollowKeyboard(3) sentinels included (Xorg Xi/getfocus.c
        // maps focus->win back to the sentinel, never resolving it).
        20 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            // NOTE: Xorg's `if (!dev->focus) return BadDevice` is NOT
            // mirrored as a keyboard-only gate: real servers initialise
            // a focus class on extension pointers too (XTS
            // Miscellaneous-3..6 call SetDeviceFocus on Devs.Button /
            // Devs.DvMod and expect Success).
            let f = crate::core_loop::xi1_focus::device_focus(state, dev);
            // Byte 1 = RepType = X_GetDeviceFocus (Xi/getfocus.c).
            let mut reply = x11::fixed_reply(byte_order, sequence, minor, 0);
            x11::write_u32(byte_order, &mut reply, f.focus); // bytes 8-11
            x11::write_u32(byte_order, &mut reply, f.time); // bytes 12-15
            reply.push(f.revert_to); // byte 16
            reply.extend_from_slice(&[0u8; 15]); // bytes 17-31: pad
            debug!(
                "client {} #{} XI1 GetDeviceFocus device={dev} -> focus=0x{:x} revert={}",
                client_id.0, sequence.0, f.focus, f.revert_to
            );
            buf.extend_from_slice(&reply);
        }
        // SetDeviceFocus (void): { focus, time, revertTo, device }.
        // focus: None(0) / PointerRoot(1) / FollowKeyboard(3) / a valid
        // viewable window. revertTo: RevertToNone(0)..FollowKeyboard(3).
        // Mirrors Xorg Xi/setfocus.c → dix SetInputFocus
        // (dix/events.c:4879): validate, then silently ignore stale /
        // future timestamps, then store + emit DeviceFocusIn/Out.
        21 => {
            if body.len() < 10 {
                return Ok(RequestOutcome::Handled);
            }
            let focus = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let req_time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let revert_to = body[8];
            let dev = u16::from(body[9]);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if revert_to > 3 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(revert_to),
                    minor,
                );
            }
            if !matches!(focus, 0 | 1 | 3) {
                let Some(window) = state.resources.window(ResourceId(focus)) else {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        focus,
                        minor,
                    );
                };
                if window.map_state != crate::resources::MapState::Viewable {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
                }
            }
            // Timestamp gate (dix/events.c:4920-4922): CurrentTime(0)
            // maps to the server clock; a time later than "now" or
            // earlier than the last-focus-change time is a silent no-op.
            let now = state.timestamp_now();
            let time = if req_time == 0 { now } else { req_time };
            let prev = crate::core_loop::xi1_focus::device_focus(state, dev);
            if crate::core_loop::xi1_focus::time_after(time, now)
                || crate::core_loop::xi1_focus::time_after(prev.time, time)
            {
                debug!(
                    "client {} #{} XI1 SetDeviceFocus device={dev} stale time {time} \
                     (now={now} last={}) — ignored",
                    client_id.0, sequence.0, prev.time
                );
                return Ok(RequestOutcome::Handled);
            }
            crate::core_loop::xi1_focus::set_device_focus(state, dev, focus, revert_to, time);
            debug!(
                "client {} #{} XI1 SetDeviceFocus device={dev} focus=0x{focus:x} \
                 revert={revert_to} time={time}",
                client_id.0, sequence.0
            );
        }
        // GetFeedbackControl: { deviceid }. yserver models one keyboard
        // control + one pointer control (the same state core
        // GetKeyboardControl/GetPointerControl expose). Report the device
        // class's single default feedback (id 0): a KbdFeedbackState for
        // key devices, a PtrFeedbackState for pointer devices — mirroring
        // Xorg Xi/getfctl.c, where the KbdFeedback ctrl *is* the keyboard
        // control.
        22 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            let feedbacks = if xi1_device_has_keys(&state.xi_devices, dev) {
                let kc = &state.keyboard_control;
                x11::encode_kbd_feedback_state(
                    byte_order,
                    0,
                    kc.bell_pitch,
                    kc.bell_duration,
                    kc.led_mask,
                    kc.global_auto_repeat,
                    kc.key_click_percent,
                    kc.bell_percent,
                    &kc.auto_repeats,
                )
            } else {
                let pc = &state.pointer_control;
                x11::encode_ptr_feedback_state(
                    byte_order,
                    0,
                    pc.accel_numerator,
                    pc.accel_denominator,
                    pc.threshold,
                )
            };
            x11::write_get_feedback_control_reply(&mut buf, byte_order, sequence, 1, &feedbacks)?;
        }
        // ChangeFeedbackControl (void): { mask, deviceid, feedbackclass }
        // followed by the class-specific control. Yserver advertises one
        // KbdFeedback (class 0/id 0) on key devices and one PtrFeedback
        // (class 1/id 0) on pointer devices. Both alias the shared core
        // keyboard/pointer control, just like Xorg.
        23 => {
            let dev = u16::from(*body.get(4).unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            let mask = u32::from_le_bytes([
                *body.first().unwrap_or(&0),
                *body.get(1).unwrap_or(&0),
                *body.get(2).unwrap_or(&0),
                *body.get(3).unwrap_or(&0),
            ]);
            let feedback_class = *body.get(5).unwrap_or(&u8::MAX);
            match feedback_class {
                0 => {
                    if body.len() != 28 {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_LENGTH,
                            0,
                            minor,
                        );
                    }
                    if !xi1_device_has_keys(&state.xi_devices, dev) || body[9] != 0 {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_MATCH,
                            0,
                            minor,
                        );
                    }
                    let mut control = state.keyboard_control.clone();
                    let mut bad_value = |value: i32| {
                        xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            value.cast_unsigned(),
                            minor,
                        )
                    };
                    if mask & (1 << 0) != 0 {
                        let value = body[14] as i8;
                        control.key_click_percent = match value {
                            -1 => crate::server::KeyboardControlState::new().key_click_percent,
                            0..=100 => value.cast_unsigned(),
                            _ => return bad_value(i32::from(value)),
                        };
                    }
                    if mask & (1 << 1) != 0 {
                        let value = body[15] as i8;
                        control.bell_percent = match value {
                            -1 => crate::server::KeyboardControlState::new().bell_percent,
                            0..=100 => value.cast_unsigned(),
                            _ => return bad_value(i32::from(value)),
                        };
                    }
                    if mask & (1 << 2) != 0 {
                        let value = i16::from_le_bytes([body[16], body[17]]);
                        control.bell_pitch = match value {
                            -1 => crate::server::KeyboardControlState::new().bell_pitch,
                            0.. => value.cast_unsigned(),
                            _ => return bad_value(i32::from(value)),
                        };
                    }
                    if mask & (1 << 3) != 0 {
                        let value = i16::from_le_bytes([body[18], body[19]]);
                        control.bell_duration = match value {
                            -1 => crate::server::KeyboardControlState::new().bell_duration,
                            0.. => value.cast_unsigned(),
                            _ => return bad_value(i32::from(value)),
                        };
                    }
                    if mask & (1 << 4) != 0 {
                        let led_mask =
                            u32::from_le_bytes(body[20..24].try_into().expect("four bytes"));
                        let led_values =
                            u32::from_le_bytes(body[24..28].try_into().expect("four bytes"));
                        control.led_mask = (control.led_mask & !led_mask) | (led_values & led_mask);
                    }
                    let key = body[12];
                    if mask & (1 << 6) != 0 {
                        if !(XI1_KEY_MIN..=XI1_KEY_MAX).contains(&key) {
                            return bad_value(i32::from(key));
                        }
                        if mask & (1 << 7) == 0 {
                            return xi1_error(
                                state,
                                client_id,
                                sequence,
                                x11::error::BAD_MATCH,
                                0,
                                minor,
                            );
                        }
                    }
                    if mask & (1 << 7) != 0 {
                        let mode = body[13];
                        if mask & (1 << 6) == 0 {
                            control.global_auto_repeat = match mode {
                                0 => false,
                                1 => true,
                                2 => crate::server::KeyboardControlState::new().global_auto_repeat,
                                _ => return bad_value(i32::from(mode)),
                            };
                        } else {
                            let index = usize::from(key >> 3);
                            let bit = 1 << (key & 7);
                            match mode {
                                0 => control.auto_repeats[index] &= !bit,
                                1 => control.auto_repeats[index] |= bit,
                                2 => {
                                    control.auto_repeats[index] = (control.auto_repeats[index]
                                        & !bit)
                                        | (crate::server::DEFAULT_AUTO_REPEATS[index] & bit);
                                }
                                _ => return bad_value(i32::from(mode)),
                            }
                        }
                    }
                    state.keyboard_control = control;
                }
                1 => {
                    if body.len() != 20 {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_LENGTH,
                            0,
                            minor,
                        );
                    }
                    if !xi1_device_has_valuators(&state.xi_devices, dev) || body[9] != 0 {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_MATCH,
                            0,
                            minor,
                        );
                    }
                    let mut control = state.pointer_control.clone();
                    let defaults = crate::server::PointerControlState::new();
                    for (bit, offset, target, default, denominator) in [
                        (
                            0,
                            14,
                            &mut control.accel_numerator,
                            defaults.accel_numerator,
                            false,
                        ),
                        (
                            1,
                            16,
                            &mut control.accel_denominator,
                            defaults.accel_denominator,
                            true,
                        ),
                        (2, 18, &mut control.threshold, defaults.threshold, false),
                    ] {
                        if mask & (1 << bit) == 0 {
                            continue;
                        }
                        let value = i16::from_le_bytes([body[offset], body[offset + 1]]);
                        if value == -1 {
                            *target = default;
                        } else if value < 0 || (denominator && value == 0) {
                            return xi1_error(
                                state,
                                client_id,
                                sequence,
                                x11::error::BAD_VALUE,
                                i32::from(value).cast_unsigned(),
                                minor,
                            );
                        } else {
                            *target = value.cast_unsigned();
                        }
                    }
                    state.pointer_control = control;
                }
                _ => {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
                }
            }
            debug!(
                "client {} #{} XI1 ChangeFeedbackControl device={dev} class={feedback_class} mask=0x{mask:x}",
                client_id.0, sequence.0,
            );
        }
        // GetDeviceKeyMapping: { deviceid, firstKeyCode, count }. Range
        // checks against the advertised 8..=255 keycode space.
        24 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            let first = *body.get(1).unwrap_or(&0);
            let count = *body.get(2).unwrap_or(&0);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_keys(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            if first < XI1_KEY_MIN {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(first),
                    minor,
                );
            }
            if u16::from(first) + u16::from(count) > 256 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(count),
                    minor,
                );
            }
            // The XI1 keyboard devices (3 master, 5 slave) share the
            // server's single physical keymap — Xorg's XkbGetCoreMap is
            // per-device, but yserver models one keyboard. Reuse the core
            // GetKeyboardMapping path so the device map matches what the
            // client sees via opcode 101.
            let (kpc, keysyms) = fetch_merged_keymap(state, backend, origin, first, count);
            x11::write_get_device_key_mapping_reply(&mut buf, byte_order, sequence, kpc, &keysyms)?;
        }
        // ChangeDeviceKeyMapping (void): { deviceid, firstKeyCode,
        // keySymsPerKeyCode, keyCodes }.
        25 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            let first = *body.get(1).unwrap_or(&0);
            let count = *body.get(3).unwrap_or(&0);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if first < XI1_KEY_MIN || u16::from(first) + u16::from(count) > 256 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(first),
                    minor,
                );
            }
            // Write the keysym rows into the shared override store the
            // read paths (minor 24 / core opcode 101) merge over the
            // backend keymap. Body: deviceid(1) firstKeyCode(1)
            // keySymsPerKeyCode(1) keyCodes(1) then keyCodes×kpk CARD32s.
            // Without this the round-trip silently dropped the change
            // (XTS XChangeDeviceKeyMapping-3).
            let kpk = *body.get(2).unwrap_or(&0);
            // Xorg XkbApplyMappingChange changes and notifies nothing for
            // zero keys.
            let xkb_change = if count == 0 {
                None
            } else {
                apply_keymap_change(
                    state,
                    backend,
                    first,
                    kpk,
                    count,
                    &body[4.min(body.len())..],
                )
            };
            // Same XKB notifications as the core request (Xorg: both reach
            // XkbApplyMappingChange): MapNotify first, ControlsNotify last.
            let xkb_event_base = backend.xkb_info().map_or(0, |(_maj, ev, _err)| ev);
            if let Some(change) = &xkb_change {
                crate::core_loop::xkb_layout::send_xkb_map_notify(
                    state,
                    xkb_event_base,
                    change.map_notify,
                );
            }
            // The core MappingNotify only when the master keyboard changed
            // (Xorg XkbSendLegacyMapNotify → XIShouldNotify), as for
            // SetDeviceModifierMapping.
            if count != 0 && dev == crate::xinput::DEVICEID_MASTER_KEYBOARD {
                legacy_keyboard_mapping_notify(state, xkb_change.as_ref(), first, count);
            }
            // ChangeDeviceKeyMapping is void (no reply), so the event
            // ordering question is moot: send to originator + others.
            // request_kind=1 = MappingKeyboard.
            if crate::core_loop::xi1_focus::xi1_client_wants_device_mapping_notify(
                state, client_id, dev,
            ) {
                let time = state.timestamp_now();
                #[allow(clippy::cast_possible_truncation)]
                let device_byte = dev as u8;
                crate::xinput::encode_xi1_device_mapping_notify(
                    &mut buf,
                    byte_order,
                    crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                    device_byte,
                    sequence,
                    time,
                    1,
                    first,
                    count,
                );
            }
            crate::core_loop::xi1_focus::emit_device_mapping_notify(
                state,
                Some(client_id),
                dev,
                1,
                first,
                count,
            );
            if let Some(change) = &xkb_change {
                crate::core_loop::xkb_layout::send_keyboard_mapping_followups(
                    state,
                    xkb_event_base,
                    change,
                    (crate::core_loop::xkb_layout::X_CHANGE_KEYBOARD_MAPPING, 0),
                );
            }
            debug!(
                "client {} #{} XI1 ChangeDeviceKeyMapping device={dev}",
                client_id.0, sequence.0
            );
        }
        // GetDeviceModifierMapping: { deviceid }. Keyboard devices
        // share the core keymap, so the reply is the same
        // modifier→keycode table the core GetModifierMapping handler
        // serves (Xorg Xi/getmmap.c reads the per-device key class —
        // ours all alias the core keyboard).
        //
        // This MUST be real data, not a zero stub: XTS's
        // Setup_Extension_DeviceInfo only treats a device as
        // modifier-capable when `max_keypermod > 0`, and a zero reply
        // silently knocked out every ModMask-gated test (GrabDeviceButton,
        // SetDeviceModifierMapping, UngrabDeviceKey-2, ... → UNTESTED).
        //
        // Wire layout (xGetDeviceModifierMappingReply, XIproto.h:1031):
        // numKeyPerModifier lives at byte 8 — NOT the core reply's
        // byte-1 data slot — followed by 8*numKeyPerModifier keycodes.
        26 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_keys(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // Prefer the per-device override stored by
            // `XSetDeviceModifierMapping`; fall back to the backend's
            // shared core map. xts5 SetDeviceModifierMapping-1 sets
            // the map and immediately reads it back.
            let (kpm, keycodes) = state
                .xi1_modifier_map
                .get(&dev)
                .cloned()
                .unwrap_or_else(|| {
                    backend
                        .get_modifier_mapping(origin)
                        .unwrap_or((0, Vec::new()))
                });
            debug_assert_eq!(keycodes.len(), 8 * usize::from(kpm));
            let length_words = u32::from(kpm) * 2; // 8*kpm bytes / 4
            // Byte 1 = RepType = X_GetDeviceModifierMapping (Xi/getmmap.c).
            let mut reply = x11::fixed_reply(byte_order, sequence, minor, length_words);
            reply.push(kpm); // byte 8: numKeyPerModifier
            reply.extend_from_slice(&[0u8; 23]); // bytes 9..=31: pad
            reply.extend_from_slice(&keycodes);
            debug!(
                "client {} #{} XI1 GetDeviceModifierMapping device={dev} kpm={kpm}",
                client_id.0, sequence.0
            );
            buf.extend_from_slice(&reply);
        }
        // SetDeviceModifierMapping: { deviceid, numKeyPerModifier,
        //   pad1, 8 × numKeyPerModifier keycodes }.
        27 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            let kpm = *body.get(1).unwrap_or(&0);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_keys(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            let need = 8usize * usize::from(kpm);
            let keycodes: Vec<u8> = (0..need).filter_map(|i| body.get(4 + i).copied()).collect();
            if keycodes.len() != need {
                return xi1_error(state, client_id, sequence, x11::error::BAD_LENGTH, 0, minor);
            }
            // Xorg ProcXSetDeviceModifierMapping → change_modmap, the core
            // request's path: a keycode outside the keycode range (xts5
            // SetDeviceModifierMapping-8 probes MinKeyCode-1) or listed
            // twice is BadValue; a held modifier key is MappingBusy.
            // Busy changed nothing and notifies nothing (Xi/setmmap.c). On
            // success the DeviceMappingNotify goes out before the XKB and
            // core notifications: to the requester (xts5 does
            // `Expect_Event` then `Expect_Reply`, so ahead of the reply too)
            // and to every other client that selected it.
            // request_kind=0 = MappingModifier.
            let notify_device = |state: &mut ServerState| {
                if crate::core_loop::xi1_focus::xi1_client_wants_device_mapping_notify(
                    state, client_id, dev,
                ) {
                    let time = state.timestamp_now();
                    #[allow(clippy::cast_possible_truncation)]
                    let device_byte = dev as u8;
                    let _dropped =
                        fanout_event_to_clients(state, &[client_id], |buf, seq, order| {
                            crate::xinput::encode_xi1_device_mapping_notify(
                                buf,
                                order,
                                crate::server::XI_FIRST_EVENT
                                    + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                                device_byte,
                                seq,
                                time,
                                0,
                                0,
                                0,
                            );
                        });
                }
                crate::core_loop::xi1_focus::emit_device_mapping_notify(
                    state,
                    Some(client_id),
                    dev,
                    0,
                    0,
                    0,
                );
            };
            let status = match change_modifier_mapping(
                state,
                backend,
                origin,
                kpm,
                &keycodes,
                Some(dev),
                notify_device,
            ) {
                ModmapChangeOutcome::Success => 0,
                ModmapChangeOutcome::Busy => 1,
                ModmapChangeOutcome::BadValue(value) => {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        value,
                        minor,
                    );
                }
            };
            // xSetDeviceModifierMappingReply: RepType @1, success @8.
            let mut reply = x11::fixed_reply(byte_order, sequence, minor, 0);
            reply.push(status);
            reply.extend_from_slice(&[0u8; 23]);
            buf.extend_from_slice(&reply);
        }
        // GetDeviceButtonMapping: { deviceid }. The map length follows the
        // device's current ButtonClass, including masters whose shape was
        // copied from their last slave; Xorg returns b->numButtons and
        // b->map (`Xi/getbmap.c:107-115`).
        28 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_buttons(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            let button_count = usize::from(
                xi1_device_button_count(&state.xi_devices, dev).expect("validated button class"),
            );
            // nElts lives at byte 8 (first byte after the reply header);
            // map bytes follow the 32-byte header, padded to 4 bytes.
            // Use the device's stored map if set (xts5
            // SetDeviceButtonMapping-1 verifies that a custom map
            // round-trips); otherwise identity.
            let mut reply = x11::fixed_reply(
                byte_order,
                sequence,
                0,
                u32::try_from(button_count.div_ceil(4)).unwrap_or(u32::MAX),
            );
            reply.push(u8::try_from(button_count).expect("XI ButtonClass fits XI1 nElts"));
            reply.extend_from_slice(&[0u8; 23]);
            let stored = state.xi1_button_map.get(&dev).cloned();
            for i in 0..button_count {
                let mapped = stored
                    .as_ref()
                    .and_then(|m| m.get(i).copied())
                    .unwrap_or_else(|| u8::try_from(i + 1).expect("XI ButtonClass fits XI1"));
                reply.push(mapped);
            }
            while !reply.len().is_multiple_of(4) {
                reply.push(0);
            }
            buf.extend_from_slice(&reply);
        }
        // SetDeviceButtonMapping: { deviceid, map_length }. Xorg passes the
        // supplied length directly to ApplyPointerMapping; it is not capped
        // by ButtonClass count (`Xi/setbmap.c:111-121`).
        29 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            let map_length = *body.get(1).unwrap_or(&0);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_buttons(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // Read the map bytes from the body (after the 4-byte
            // header that holds deviceid/map_length/pad). Xorg's
            // ApplyPointerMapping only checks device access and whether a
            // changed held button makes the mapping busy, then copies the
            // supplied bytes (`dix/inpututils.c:43-124`).
            let map_bytes: Vec<u8> = (0..usize::from(map_length))
                .filter_map(|i| body.get(4 + i).copied())
                .collect();
            let current_map = state.xi1_button_map.get(&dev);
            let buttons_down = state
                .xi1_device_input_state
                .get(&dev)
                .map(|device_state| &device_state.buttons_down);
            let mapping_busy = map_bytes.iter().enumerate().any(|(index, new_mapping)| {
                let button = index + 1;
                let previous_mapping = current_map
                    .and_then(|mapping| mapping.get(index))
                    .copied()
                    .unwrap_or_else(|| u8::try_from(button).unwrap_or(u8::MAX));
                let button_is_down = buttons_down.is_some_and(|down| {
                    down.get(button / 8)
                        .is_some_and(|byte| byte & (1 << (button % 8)) != 0)
                });
                previous_mapping != *new_mapping && button_is_down
            });
            if mapping_busy {
                // Xorg ApplyPointerMapping returns MappingBusy before copying
                // the map or emitting MappingNotify (`dix/inpututils.c:62-65`).
                let mut reply = xi1_zero_reply(byte_order, sequence);
                reply[8] = 1; // MappingBusy
                buf.extend_from_slice(&reply);
            } else {
                // Xorg copies exactly the supplied prefix into the device's
                // existing map (`dix/inpututils.c:78-80`). Seed an implicit
                // identity map from the current ButtonClass, then retain any
                // tail when a shorter prefix is written.
                let button_count = usize::from(
                    xi1_device_button_count(&state.xi_devices, dev)
                        .expect("validated button class"),
                );
                let mut updated = state.xi1_button_map.get(&dev).cloned().unwrap_or_else(|| {
                    (1..=button_count)
                        .map(|button| u8::try_from(button).unwrap_or(u8::MAX))
                        .collect()
                });
                while updated.len() < map_bytes.len() {
                    updated.push(u8::try_from(updated.len() + 1).unwrap_or(u8::MAX));
                }
                updated[..map_bytes.len()].copy_from_slice(&map_bytes);
                state.xi1_button_map.insert(dev, updated);
                // Reply first, then DeviceMappingNotify event in the same
                // outbound write. request_kind=2 = MappingPointer.
                buf.extend_from_slice(&xi1_zero_reply(byte_order, sequence));
                if crate::core_loop::xi1_focus::xi1_client_wants_device_mapping_notify(
                    state, client_id, dev,
                ) {
                    let time = state.timestamp_now();
                    #[allow(clippy::cast_possible_truncation)]
                    let device_byte = dev as u8;
                    crate::xinput::encode_xi1_device_mapping_notify(
                        &mut buf,
                        byte_order,
                        crate::server::XI_FIRST_EVENT
                            + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
                        device_byte,
                        sequence,
                        time,
                        2,
                        0,
                        0,
                    );
                }
                crate::core_loop::xi1_focus::emit_device_mapping_notify(
                    state,
                    Some(client_id),
                    dev,
                    2,
                    0,
                    0,
                );
            }
        }
        // QueryDeviceState: { deviceid }. Snapshot of the device's
        // key / button / valuator state (Xorg reads current classes at
        // `Xi/queryst.c:131-157`) — the same
        // source data DeviceStateNotify reports, in the request-reply
        // envelope: num_classes at reply byte 8, then xKeyState /
        // xButtonState (36 bytes each: class, length, count, pad,
        // 32-byte down-bitmask) and xValuatorState (4 bytes + one
        // INT32 per axis). `xinput query-state` consumes this.
        //
        // num_keys here is max_key_code - min_key_code + 1 (248) —
        // ONE MORE than the deviceStateNotify event's max-min (247);
        // Xorg is inconsistent between the two and clients are built
        // against that, so mirror it.
        30 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            let dev_state = state
                .xi1_device_input_state
                .get(&dev)
                .copied()
                .unwrap_or_default();
            let class_shape = state
                .xi_devices
                .device(dev)
                .expect("validated XI device")
                .class_shape;
            let mut data: Vec<u8> = Vec::new();
            let mut num_classes = 0u8;
            if xi1_device_has_keys(&state.xi_devices, dev) {
                // xKeyState: keycodes 8..=255 → num_keys = 248.
                data.push(0); // class = KeyClass
                data.push(36); // length
                data.push(248); // num_keys (max - min + 1)
                data.push(0); // pad
                data.extend_from_slice(&dev_state.keys_down);
                num_classes += 1;
            }
            if xi1_device_has_buttons(&state.xi_devices, dev) {
                data.push(1); // class = ButtonClass
                data.push(36); // length
                data.push(class_shape.button_count()); // num_buttons
                data.push(0); // pad
                data.extend_from_slice(&dev_state.buttons_down);
                num_classes += 1;
            }
            if xi1_device_has_valuators(&state.xi_devices, dev) {
                let valuator_count = usize::from(class_shape.valuator_count());
                let valuator_length = 4 + 4 * valuator_count;
                data.push(2); // class = ValuatorClass
                data.push(u8::try_from(valuator_length).expect("XI valuator block fits XI1"));
                data.push(class_shape.valuator_count()); // num_valuators
                data.push(dev_state.valuator_mode); // mode (in-proximity)
                // Stored axis values (Xorg axisVal): axes 0/1 track
                // the sprite under real motion; fakes write their
                // explicit payload.
                for value in dev_state.valuators.iter().take(valuator_count) {
                    x11::write_u32(byte_order, &mut data, value.cast_unsigned());
                }
                num_classes += 1;
            }
            debug_assert_eq!(data.len() % 4, 0);
            let length_words = u32::try_from(data.len() / 4).unwrap_or(u32::MAX);
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, length_words);
            reply.push(num_classes); // byte 8
            reply.extend_from_slice(&[0u8; 23]); // bytes 9..=31: pad
            reply.extend_from_slice(&data);
            debug!(
                "client {} #{} XI1 QueryDeviceState device={dev} classes={num_classes}",
                client_id.0, sequence.0
            );
            buf.extend_from_slice(&reply);
        }
        // SendExtensionEvent (void): { destination, deviceid, propagate,
        // count, num_events } + events (32 bytes each) + classes.
        31 => {
            if body.len() < 12 {
                return Ok(RequestOutcome::Handled);
            }
            let dest = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let dev = u16::from(body[4]);
            let class_count = usize::from(u16::from_le_bytes([body[6], body[7]]));
            let num_events = usize::from(body[8]);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            // Xorg Xi/sendexev.c order: device, then the class list
            // (CreateMaskFromList → BadClass), and only then the
            // destination window inside SendEvent — XTS
            // XSendExtensionEvent-20 sends a stale window + bogus class
            // and expects the BadClass.
            let classes_off = 12 + num_events * 32;
            // The XI swap table for minor 31 marks everything past the
            // header as `OpaqueTail` because the per-event payload
            // (the sender's byte order, like core SendEvent's xEvent
            // template) must NOT be swapped. That leaves the trailing
            // XEventClass[] in the client's native order; read each
            // class with `byte_order` awareness rather than blind LE.
            let read_class = |off: usize| -> Option<u32> {
                let bytes = body.get(off..off + 4)?;
                let arr = [bytes[0], bytes[1], bytes[2], bytes[3]];
                Some(match byte_order {
                    yserver_protocol::x11::ClientByteOrder::LittleEndian => u32::from_le_bytes(arr),
                    yserver_protocol::x11::ClientByteOrder::BigEndian => u32::from_be_bytes(arr),
                })
            };
            for i in 0..class_count {
                let off = classes_off + i * 4;
                let Some(class) = read_class(off) else {
                    break;
                };
                if !xi1_device_valid(&state.xi_devices, xi1_event_class_device(class)) {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_CLASS,
                        class,
                        minor,
                    );
                }
            }
            // Like core SendEvent, destination accepts the specials
            // PointerWindow (0) and InputFocus (1).
            if !matches!(dest, 0 | 1) && !xi1_window_exists(state, dest) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    dest,
                    minor,
                );
            }
            // Per Xorg `Xi/sendexev.c::ProcXSendExtensionEvent` →
            // `Xi/exevents.c::SendEvent`: resolve the destination,
            // then deliver each supplied event to (a) clients whose
            // XI1 selection on the destination window intersects the
            // request's class list, and (b) the window's CREATOR when
            // the per-device mask is empty (the `noextensioneventclass`
            // case — Xorg `DeliverToWindowOwner` short-circuits on
            // `filter == CantBeFiltered` = `NoEventMask` = 0). The
            // class-intersection rule preserves XI1 event chains
            // (DeviceKeyPress + DeviceValuator continuation), which
            // the client selects via the head class only.
            // Propagate semantics are not yet honoured — direct-window
            // match only, like core SendEvent with propagate=False.
            //
            // Specials (xserver.git Xi/exevents.c:2915-2944):
            //  - PointerWindow → sprite window (deepest mapped window
            //    under the pointer);
            //  - InputFocus → device's focus window, with PointerRoot
            //    falling back to the sprite and FollowKeyboard chasing
            //    the core focus inside `resolve_focus`.
            let dest_window: Option<ResourceId> = match dest {
                0 => Some(crate::core_loop::key_fanout::deepest_window_at_pointer(
                    state,
                )),
                1 => {
                    let focus = crate::core_loop::xi1_focus::device_focus(state, dev).focus;
                    match crate::core_loop::xi1_focus::resolve_focus(state, focus) {
                        crate::core_loop::xi1_focus::Xi1FocusTarget::None => None,
                        crate::core_loop::xi1_focus::Xi1FocusTarget::PointerRoot => Some(
                            crate::core_loop::key_fanout::deepest_window_at_pointer(state),
                        ),
                        crate::core_loop::xi1_focus::Xi1FocusTarget::Window(f) => {
                            // Xorg `effectiveFocus` rule (xserver.git
                            // Xi/exevents.c:2936-2941): if the focus is
                            // an ancestor of (or equal to) the sprite,
                            // deliver to the sprite — the focus
                            // bounds propagation but the deepest
                            // pointer window is the actual target.
                            // Otherwise the sprite is outside the focus
                            // subtree and delivery goes to the focus
                            // directly. xts5 XSendExtensionEvent-3
                            // probes the inferior-of-focus case.
                            let sprite =
                                crate::core_loop::key_fanout::deepest_window_at_pointer(state);
                            if f == sprite
                                || crate::core_loop::xi1_focus::is_ancestor(state, f, sprite)
                            {
                                Some(sprite)
                            } else {
                                Some(f)
                            }
                        }
                    }
                }
                w => Some(ResourceId(w)),
            };
            if let Some(dest_window) = dest_window {
                // Collect the request's class list (already
                // validated above for `xi1_device_valid`). Same
                // byte-order reasoning as the BadClass loop above —
                // classes are in the sender's native order because the
                // XI swap table can't carve them out of the variable
                // event payload that precedes them.
                let mut request_classes: Vec<u32> = Vec::with_capacity(class_count);
                for i in 0..class_count {
                    let off = classes_off + i * 4;
                    let Some(class) = read_class(off) else {
                        break;
                    };
                    request_classes.push(class);
                }
                // Xorg `Xi/grabdev.c::CreateMaskFromList` walks the
                // class list and OR's per-class mask bits into the
                // per-device mask. `noextensioneventclass` (low byte
                // `_noExtensionEvent = 9` per XI.h:260) has no
                // associated mask bit, so it contributes nothing —
                // a request whose only classes are noextensioneventclass
                // entries (or an empty list) ends up with an
                // all-zero mask, which `DeliverToWindowOwner` treats
                // as `CantBeFiltered` and delivers to the window
                // creator unconditionally. Without this fall-through
                // xts5 XSendExtensionEvent 1–4 (which always pass
                // `noextensioneventclass`) fail because the test
                // process never calls XSelectExtensionEvent first.
                const XI1_NO_EXTENSION_EVENT_OFFSET: u8 = 9;
                #[allow(clippy::cast_possible_truncation)]
                let mask_is_zero = request_classes.is_empty()
                    || request_classes
                        .iter()
                        .all(|c| (c & 0xff) as u8 == XI1_NO_EXTENSION_EVENT_OFFSET);
                let propagate = matches!(body.get(5), Some(&p) if p != 0);
                let targets = xi1_send_extension_event_resolve_targets(
                    state,
                    dest_window,
                    &request_classes,
                    mask_is_zero,
                    propagate,
                );
                if !targets.is_empty() {
                    // Pre-snapshot all event bytes so the fanout
                    // closure can borrow them per-recipient.
                    let mut events_bytes: Vec<[u8; 32]> = Vec::with_capacity(num_events);
                    for i in 0..num_events {
                        let off = 12 + i * 32;
                        let Some(ev) = body.get(off..off + 32) else {
                            break;
                        };
                        events_bytes.push(ev.try_into().expect("32-byte slice"));
                    }
                    let targets_vec: Vec<ClientId> = targets.into_iter().collect();
                    let _dropped =
                        fanout_event_to_clients(state, &targets_vec, |buf, seq, order| {
                            for ev_bytes in &events_bytes {
                                let mut out = *ev_bytes;
                                out[0] |= 0x80; // send_event
                                let seq_bytes = match order {
                                    yserver_protocol::x11::ClientByteOrder::LittleEndian => {
                                        seq.0.to_le_bytes()
                                    }
                                    yserver_protocol::x11::ClientByteOrder::BigEndian => {
                                        seq.0.to_be_bytes()
                                    }
                                };
                                out[2] = seq_bytes[0];
                                out[3] = seq_bytes[1];
                                buf.extend_from_slice(&out);
                            }
                        });
                }
            }
            debug!(
                "client {} #{} XI1 SendExtensionEvent device={dev}",
                client_id.0, sequence.0
            );
        }
        // DeviceBell (void): { deviceid, feedbackid, feedbackclass,
        // percent }. Yserver exposes KbdFeedback id 0 on key devices but has
        // no bell procedure, and exposes no BellFeedback. Xorg
        // `Xi/devbell.c::ProcXDeviceBell` returns BadValue for both cases;
        // unlike core Bell, this is not a successful bell-less no-op.
        32 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            let feedback_id = *body.get(1).unwrap_or(&0);
            let feedback_class = *body.get(2).unwrap_or(&0);
            #[allow(clippy::cast_possible_wrap)]
            let percent = *body.get(3).unwrap_or(&0) as i8;
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            // Xorg validates percent before looking up the feedback.
            if !(-100..=100).contains(&percent) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(*body.get(3).unwrap_or(&0)),
                    minor,
                );
            }
            if !matches!(feedback_class, 0 | 5) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(feedback_class),
                    minor,
                );
            }
            let feedback_exists = feedback_class == 0
                && feedback_id == 0
                && xi1_device_has_keys(&state.xi_devices, dev);
            if !feedback_exists {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(feedback_id),
                    minor,
                );
            }
            // The feedback exists, but its BellProc is absent.
            return xi1_error(state, client_id, sequence, x11::error::BAD_VALUE, 0, minor);
        }
        // SetDeviceValuators: { deviceid, first_valuator, num_valuators,
        //   pad1, INT32 values[num_valuators] }. Body layout follows
        // `xSetDeviceValuatorsReq` (XIproto.h:1144-1153).
        33 => {
            let dev = u16::from(*body.first().unwrap_or(&0));
            let first = *body.get(1).unwrap_or(&0);
            let num = *body.get(2).unwrap_or(&0);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if !xi1_device_has_valuators(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // `IsXTestDevice` rejects the XTEST pointer with BadMatch,
            // ahead of the axis-range check (`Xi/setdval.c:113-114`).
            if dev == crate::xinput::DEVICEID_XTEST_POINTER {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            let axis_count = xi1_device_valuator_count(&state.xi_devices, dev)
                .expect("validated valuator class");
            if u16::from(first) + u16::from(num) > u16::from(axis_count) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(num),
                    minor,
                );
            }
            // Xorg checks against `dev->valuator->numAxes`
            // (`Xi/setdval.c:110-117`) and
            // `ProcXSetDeviceValuators` returns
            // `AlreadyGrabbed` when another client holds an active
            // grab on the device. `xSetDeviceValuatorsReply.status`
            // sits at byte 8 (right after the 8-byte standard reply
            // header) — XIproto.h:1249-1261.
            const ALREADY_GRABBED: u8 = 1;
            let grabbed_elsewhere = state
                .xi1_active_grabs
                .get(&dev)
                .is_some_and(|g| g.owner != client_id);
            if grabbed_elsewhere {
                let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
                reply.push(ALREADY_GRABBED); // byte 8: status
                reply.extend_from_slice(&[0u8; 23]); // bytes 9-31: pad
                buf.extend_from_slice(&reply);
            } else {
                // Persist the new axis values into
                // `xi1_device_input_state` so the next
                // DeviceStateNotify / QueryDeviceState reports them.
                // xts5 XSetDeviceValuators 1 / 2 read each axis back
                // immediately after the set and expect the supplied
                // value verbatim.
                let entry = state.xi1_device_input_state.entry(dev).or_default();
                for i in 0..usize::from(num) {
                    let off = 4 + i * 4;
                    let Some(slice) = body.get(off..off + 4) else {
                        break;
                    };
                    let value = i32::from_le_bytes(slice.try_into().expect("4-byte slice"));
                    let idx = usize::from(first) + i;
                    if let Some(slot) = entry.valuators.get_mut(idx) {
                        *slot = value;
                    }
                }
                buf.extend_from_slice(&xi1_zero_reply(byte_order, sequence));
            }
        }
        // GetDeviceControl: { control, deviceid }. DEVICE_RESOLUTION(1)
        // is the only control defined for XI 1.x core; it needs
        // valuators. Reply layout (`xGetDeviceControlReply` +
        // `xDeviceResolutionState`, XIproto.h):
        //   bytes 0..32  standard reply header (length in 32-bit
        //                units of the trailing buf)
        //   bytes 32..40 xDeviceResolutionState header:
        //                control(2)=1, length(2)=trailing-bytes,
        //                num_valuators(4)
        //   bytes 40..   3 × num_valuators × CARD32 — resolutions,
        //                min_resolutions, max_resolutions.
        // Xorg sizes and serializes this from `v->numAxes`
        // (`Xi/getdctl.c:192-219`).
        // xts5 ChangeDeviceControl-1/-2 set + read back via
        // GetDeviceControl; without a real reply the test SEGFAULTs.
        34 => {
            let control =
                u16::from_le_bytes([*body.first().unwrap_or(&0), *body.get(1).unwrap_or(&0)]);
            let dev = u16::from(*body.get(2).unwrap_or(&0));
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if control != 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(control),
                    minor,
                );
            }
            if !xi1_device_has_valuators(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            let num_axes = usize::from(
                xi1_device_valuator_count(&state.xi_devices, dev)
                    .expect("validated valuator class"),
            );
            let stored = state.xi1_resolution.get(&dev).cloned();
            let resolutions: Vec<[i32; 3]> = (0..num_axes)
                .map(|i| {
                    stored
                        .as_ref()
                        .and_then(|v| v.get(i).copied())
                        .unwrap_or([0, 0, 0])
                })
                .collect();
            // 8 bytes xDeviceResolutionState header + 3 × num_axes × 4
            let trailing_bytes = 8 + 3 * num_axes * 4;
            let length_units = u32::try_from(trailing_bytes / 4).unwrap_or(u32::MAX);
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, length_units);
            // Pad bytes 8..32 of the standard reply header.
            reply.extend_from_slice(&[0u8; 24]);
            // xDeviceResolutionState header
            x11::write_u16(byte_order, &mut reply, 1); // control = DEVICE_RESOLUTION
            #[allow(clippy::cast_possible_truncation)]
            let ctl_len = trailing_bytes as u16;
            x11::write_u16(byte_order, &mut reply, ctl_len);
            x11::write_u32(byte_order, &mut reply, u32::try_from(num_axes).unwrap_or(0));
            // Three runs over axes: resolution, min_resolution, max_resolution.
            for r in &resolutions {
                x11::write_u32(byte_order, &mut reply, r[0].cast_unsigned());
            }
            for r in &resolutions {
                x11::write_u32(byte_order, &mut reply, r[1].cast_unsigned());
            }
            for r in &resolutions {
                x11::write_u32(byte_order, &mut reply, r[2].cast_unsigned());
            }
            buf.extend_from_slice(&reply);
        }
        // ChangeDeviceControl: { control, deviceid } + xDeviceCtl
        // { control, length, first_valuator, num_valuators, pad2,
        //   resolutions[] }.
        35 => {
            if body.len() < 4 {
                return Ok(RequestOutcome::Handled);
            }
            let control = u16::from_le_bytes([body[0], body[1]]);
            let dev = u16::from(body[2]);
            if !xi1_device_valid(&state.xi_devices, dev) {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    XI1_ERROR_BAD_DEVICE,
                    u32::from(dev),
                    minor,
                );
            }
            if control != 1 {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(control),
                    minor,
                );
            }
            // Embedded xDeviceCtl follows at body[4..]; its control id
            // must agree with the request's (Xorg Xi/chgdctl.c →
            // BadMatch on mismatch).
            if body.len() >= 6 {
                let ctl_control = u16::from_le_bytes([body[4], body[5]]);
                if ctl_control != control {
                    return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
                }
            }
            if !xi1_device_has_valuators(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
            // Xorg initializes relative axes without physical resolution
            // metadata as resolution/min_resolution/max_resolution = 0/0/0
            // (`dix/devices.c::InitValuatorClassDeviceStruct`). Yserver's
            // synthetic relative axes do the same, so zero is the sole valid
            // value and round-trips through GetDeviceControl; nonzero values
            // are correctly outside the advertised range.
            let mut staged: Vec<(usize, i32)> = Vec::new();
            if body.len() >= 10 {
                let first = body[8];
                let num = body[9];
                // Xorg rejects ranges past the current ValuatorClass
                // (`Xi/chgdctl.c:157-160`).
                let axis_count = xi1_device_valuator_count(&state.xi_devices, dev)
                    .expect("validated valuator class");
                if u16::from(first) + u16::from(num) > u16::from(axis_count) {
                    return xi1_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        u32::from(num),
                        minor,
                    );
                }
                for i in 0..usize::from(num) {
                    let off = 12 + i * 4;
                    if off + 4 > body.len() {
                        break;
                    }
                    let res = i32::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                    ]);
                    if res != 0 {
                        return xi1_error(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            res.cast_unsigned(),
                            minor,
                        );
                    }
                    staged.push((usize::from(first) + i, res));
                }
            }
            // Xorg `Xi/chgdctl.c::ProcXChangeDeviceControl` returns
            // `AlreadyGrabbed` (status byte 8) when another client
            // holds an active grab on the device — xts5
            // ChangeDeviceControl-10.
            const ALREADY_GRABBED: u8 = 1;
            let grabbed_elsewhere = state
                .xi1_active_grabs
                .get(&dev)
                .is_some_and(|g| g.owner != client_id);
            let status = if grabbed_elsewhere {
                ALREADY_GRABBED
            } else {
                // Persist the validated resolutions so the matching
                // `XGetDeviceControl` reports them.
                if !staged.is_empty() {
                    let entry = state.xi1_resolution.entry(dev).or_insert_with(|| {
                        vec![
                            [0, 0, 0];
                            usize::from(
                                xi1_device_valuator_count(&state.xi_devices, dev)
                                    .expect("validated valuator class")
                            )
                        ]
                    });
                    for (axis, value) in &staged {
                        if let Some(row) = entry.get_mut(*axis) {
                            row[0] = *value;
                        }
                    }
                }
                0
            };
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            reply.push(status);
            reply.extend_from_slice(&[0u8; 23]);
            buf.extend_from_slice(&reply);
        }
        50 => {
            // XIGetFocus { deviceid:CARD16 } -> xXIGetFocusReply
            // { focus:WINDOW @8, 20 bytes pad }. Xorg ProcXIGetFocus
            // (Xi/xisetdevfocus.c) reports the device's raw focus (None=0 /
            // PointerRoot=1 / FollowKeyboard=3 / window) without resolving
            // the sentinels. Only keyboards have a focus class; pointers
            // and unknown ids are BadDevice with no errorValue. The master
            // keyboard's focus is the core focus; the slave keyboard's is
            // its own (shared with XI1 GetDeviceFocus).
            let dev = if body.len() >= 2 {
                u16::from_le_bytes([body[0], body[1]])
            } else {
                0
            };
            if !xi1_device_has_keys(&state.xi_devices, dev) {
                return xi1_error(state, client_id, sequence, XI1_ERROR_BAD_DEVICE, 0, minor);
            }
            let focus = if state.xi_devices.role(dev)
                == Some(crate::xinput::XiDeviceRole::MasterKeyboard)
            {
                state.core_focus.raw
            } else {
                crate::core_loop::xi1_focus::device_focus(state, dev).focus
            };
            let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
            x11::write_u32(byte_order, &mut reply, focus); // bytes 8-11
            reply.extend_from_slice(&[0u8; 20]); // bytes 12-31 pad
            debug!(
                "client {} #{} XIGetFocus device={dev} -> focus=0x{focus:x}",
                client_id.0, sequence.0
            );
            buf.extend_from_slice(&reply);
        }
        61 => {
            if let Some(entries) = x11::parse_xi_barrier_release(body) {
                for (deviceid, barrier_id, event_id) in entries {
                    if !(deviceid == 0 || deviceid == 1 || deviceid == 2) {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            XI1_ERROR_BAD_DEVICE,
                            u32::from(deviceid),
                            61,
                            XI2_MAJOR_OPCODE,
                        );
                    }
                    let Some(barrier) = state.pointer_barriers.get_mut(&barrier_id) else {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            barrier_id,
                            61,
                            XI2_MAJOR_OPCODE,
                        );
                    };
                    if barrier.owner != client_id {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_ACCESS,
                            barrier_id,
                            61,
                            XI2_MAJOR_OPCODE,
                        );
                    }
                    if barrier.event_id == event_id {
                        barrier.release_event_id = event_id;
                    }
                }
            }
        }
        41 => {
            return handle_xi_warp_pointer(state, backend, origin, client_id, sequence, body);
        }
        43 => {
            return handle_xi_change_hierarchy(state, client_id, sequence, byte_order, body);
        }
        49 => return handle_xi_set_focus(state, client_id, sequence, body),
        // Every minor in 1..=XINPUT_LAST_REQUEST has an arm above; like
        // Xorg's ProcIDispatch, anything else is BadRequest.
        _ => {
            debug_assert!(minor == 0 || minor > XINPUT_LAST_REQUEST);
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(minor),
                header.opcode,
            );
        }
    }
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    Ok(write_to_client(client, client_id, &buf))
}
