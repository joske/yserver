use crate::{
    access::{AccessMode, RequestOpcodes},
    core_loop::process_request::{
        RequestOutcome, XI2_FIRST_ERROR, XI2_MAJOR_OPCODE, XI2_SERVER_MAJOR_VERSION,
        XI2_SERVER_MINOR_VERSION, emit_x11_error_with_minor,
        xi::{
            XI1_ERROR_BAD_DEVICE, XI1_KEY_MAX, XI1_KEY_MIN, dispatch::xi_write_reply,
            record_xi2_client_version, xi1_device_has_keys, xi1_device_has_valuators,
            xi1_device_is_master, xi1_device_valid, xi1_error, xi1_zero_reply,
        },
    },
    server::{ServerState, XI_FIRST_EVENT},
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber},
};

pub(super) fn handle_xi1_get_extension_version(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
    xi_write_reply(state, client_id, buf)
}

#[allow(clippy::needless_return)]
pub(super) fn handle_xi_set_client_pointer(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    if body.len() >= 4 {
        let id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_arbitrary_resource_if_present(
                state,
                client_id,
                id,
                AccessMode::MANAGE,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
    }
    debug!("client {} #{} XISetClientPointer", client_id.0, sequence.0);
    return Ok(RequestOutcome::Handled);
}

pub(super) fn handle_xi_get_client_pointer(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    if body.len() >= 4 {
        let id = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_arbitrary_resource_if_present(
                state,
                client_id,
                id,
                AccessMode::GET_ATTR,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
    }
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
    xi_write_reply(state, client_id, buf)
}

pub(super) fn handle_xi_query_version(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
    xi_write_reply(state, client_id, buf)
}

pub(super) fn handle_xi_query_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
        let reply = crate::xinput::query::encode_reply(byte_order, sequence, &devices, class_data)?;
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// XI 1.x ListInputDevices (minor 2). It selects the same live
// registry entries as XIQueryDevice. A previous empty-list stub
// crashed Chromium/Electron: Ozone-X11 cross-checks XI1's
// ListInputDevices against XI2's XIQueryDevice and fatal-CHECKs
// when XI2 has a master pointer but XI1 reports zero devices.
pub(super) fn handle_xi1_list_input_devices(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
                        crate::xinput::XiFacetKind::Keyboard => (3, keyboard_atom, &classes[..]),
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
    xi_write_reply(state, client_id, buf)
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
pub(super) fn handle_xi1_open_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
    xi_write_reply(state, client_id, buf)
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
pub(super) fn handle_xi1_close_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
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
    xi_write_reply(state, client_id, Vec::new())
}

// SetDeviceMode: { deviceid, mode }. Mode changes only make
// sense for devices with valuators (Xorg Xi/setmode.c).
pub(super) fn handle_xi1_set_device_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
    xi_write_reply(state, client_id, buf)
}

// ChangeKeyboardDevice: { deviceid }. Needs a device with keys
// (Xorg Xi/chgkbd.c). xts5 expects:
//   1. ChangeDeviceNotify (request=1 = NewKeyboard)
//   2. core MappingNotify (request=1 = MappingKeyboard)
//   3. reply
// — in that order on the wire.
pub(super) fn handle_xi1_change_keyboard_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
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
        if crate::core_loop::xi1_focus::xi1_client_wants_change_device_notify(state, client_id, dev)
        {
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
    xi_write_reply(state, client_id, buf)
}

// ChangePointerDevice is unsupported by Xorg 21.1.24: its handler
// returns BadDevice after request-size validation (Xi/chgptr.c:92-98).
#[allow(clippy::needless_return)]
pub(super) fn handle_xi1_change_pointer_device(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    minor: u8,
) -> io::Result<RequestOutcome> {
    return xi1_error(state, client_id, sequence, XI1_ERROR_BAD_DEVICE, 0, minor);
}
