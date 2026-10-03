//! XI2 device-query selection and wire encoding.

use std::io::{self, Error, ErrorKind};

use yserver_protocol::x11::{self, AtomId, ClientByteOrder, SequenceNumber};

use super::{DEVICEID_MASTER_KEYBOARD, DEVICEID_MASTER_POINTER, XiDevice, XiFacetKind};

/// XI2 device selectors use the same XInput `BadDevice` error for an
/// unrecognized exact ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiQueryError {
    BadDevice(u16),
}

/// Atom labels and last-reported values needed by pointer class encoding.
#[derive(Debug, Clone, Copy)]
pub(crate) struct XiQueryClassData {
    pub button_labels: [AtomId; 7],
    pub axis_labels: [AtomId; 4],
    pub pointer: (i32, i32),
    pub scroll: [i32; 2],
}

#[derive(Debug, Clone, Copy)]
enum DeviceClass {
    Pointer,
    Keyboard,
}

/// Encode a registry-selected XI2 device list as an `XIQueryDevice` reply.
/// Every multi-byte reply and class field follows the requesting client's
/// byte order. Class values and shapes preserve yserver's existing master
/// pointer and GDK-compatible generic slave-pointer descriptors.
pub(crate) fn encode_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    devices: &[&XiDevice],
    class_data: XiQueryClassData,
) -> io::Result<Vec<u8>> {
    let mut infos = Vec::new();

    for device in devices {
        let (use_type, attachment, class) = device_descriptor(device)?;
        let (classes, num_classes) = match class {
            DeviceClass::Pointer => build_pointer_classes(
                byte_order,
                device.id,
                if device.id == super::DEVICEID_MASTER_POINTER {
                    class_data
                } else {
                    XiQueryClassData {
                        scroll: device.scroll_axis_values,
                        ..class_data
                    }
                },
            ),
            DeviceClass::Keyboard => build_key_classes(byte_order, device.id),
        };
        write_device_info(
            byte_order,
            &mut infos,
            device,
            use_type,
            attachment,
            &classes,
            num_classes,
        )?;
    }

    let num_devices = u16::try_from(devices.len())
        .map_err(|_| Error::new(ErrorKind::InvalidData, "too many XI devices"))?;
    let reply_length = u32::from(x11::checked_units(infos.len())?);
    let mut reply = x11::fixed_reply(byte_order, sequence, 48, reply_length);
    x11::write_u16(byte_order, &mut reply, num_devices);
    reply.extend_from_slice(&[0; 22]);
    reply.extend_from_slice(&infos);
    Ok(reply)
}

fn device_descriptor(device: &XiDevice) -> io::Result<(u16, u16, DeviceClass)> {
    match device.id {
        DEVICEID_MASTER_POINTER => Ok((1, DEVICEID_MASTER_KEYBOARD, DeviceClass::Pointer)),
        DEVICEID_MASTER_KEYBOARD => Ok((2, DEVICEID_MASTER_POINTER, DeviceClass::Keyboard)),
        super::DEVICEID_XTEST_POINTER => Ok(slave_descriptor(
            device.attached_master,
            3,
            DeviceClass::Pointer,
        )),
        super::DEVICEID_XTEST_KEYBOARD => Ok(slave_descriptor(
            device.attached_master,
            4,
            DeviceClass::Keyboard,
        )),
        _ => match device.facet {
            Some(XiFacetKind::PointerTouch) => Ok(slave_descriptor(
                device.attached_master,
                3,
                DeviceClass::Pointer,
            )),
            Some(XiFacetKind::Keyboard) => Ok(slave_descriptor(
                device.attached_master,
                4,
                DeviceClass::Keyboard,
            )),
            None => Err(Error::new(
                ErrorKind::InvalidData,
                format!("XI device {} has no protocol role", device.id),
            )),
        },
    }
}

fn slave_descriptor(
    attached_master: Option<u16>,
    slave_use: u16,
    class: DeviceClass,
) -> (u16, u16, DeviceClass) {
    match attached_master {
        Some(master) => (slave_use, master, class),
        None => (5, 0, class), // XIFloatingSlave has no defined attachment.
    }
}

fn write_device_info(
    byte_order: ClientByteOrder,
    out: &mut Vec<u8>,
    device: &XiDevice,
    use_type: u16,
    attachment: u16,
    classes: &[u8],
    num_classes: u16,
) -> io::Result<()> {
    let name = device.name.as_bytes();
    let name_len = u16::try_from(name.len()).map_err(|_| {
        Error::new(
            ErrorKind::InvalidData,
            format!("XI device {} name is too long", device.id),
        )
    })?;
    x11::write_u16(byte_order, out, device.id);
    x11::write_u16(byte_order, out, use_type);
    x11::write_u16(byte_order, out, attachment);
    x11::write_u16(byte_order, out, num_classes);
    x11::write_u16(byte_order, out, name_len);
    out.push(u8::from(device.enabled));
    out.push(0);
    out.extend_from_slice(name);
    x11::pad_vec4(out);
    out.extend_from_slice(classes);
    Ok(())
}

pub(crate) fn build_pointer_classes(
    byte_order: ClientByteOrder,
    source_id: u16,
    data: XiQueryClassData,
) -> (Vec<u8>, u16) {
    // GDK builds its seat/device table from the XI2 hierarchy and probes the
    // first slave pointer for libinput-style properties. Keep the generic
    // button/valuator/scroll shape for compatibility; this does not assign
    // physical ownership or properties to virtual XTEST device 4.
    // The two scroll valuators must remain declared: GDK's scroll-valuator
    // setup asserts that each ScrollClass axis is below the valuator count.
    let mut classes = Vec::new();
    write_button_class(byte_order, &mut classes, source_id, &data.button_labels);

    write_valuator_class(
        byte_order,
        &mut classes,
        source_id,
        0,
        data.axis_labels[0],
        -1,
        -1,
        0,
        data.pointer.0,
    );
    write_valuator_class(
        byte_order,
        &mut classes,
        source_id,
        1,
        data.axis_labels[1],
        -1,
        -1,
        0,
        data.pointer.1,
    );
    write_valuator_class(
        byte_order,
        &mut classes,
        source_id,
        2,
        data.axis_labels[2],
        -1,
        0,
        0,
        data.scroll[0],
    );
    write_valuator_class(
        byte_order,
        &mut classes,
        source_id,
        3,
        data.axis_labels[3],
        -1,
        0,
        0,
        data.scroll[1],
    );
    write_scroll_class(byte_order, &mut classes, source_id, 2, 1);
    write_scroll_class(byte_order, &mut classes, source_id, 3, 2);
    (classes, 7)
}

fn write_button_class(
    byte_order: ClientByteOrder,
    out: &mut Vec<u8>,
    source_id: u16,
    labels: &[AtomId],
) {
    let num_buttons = u16::try_from(labels.len()).unwrap_or(u16::MAX);
    let state_words = num_buttons.div_ceil(32) as usize;
    let byte_len = 8 + 4 * state_words + 4 * usize::from(num_buttons);
    x11::write_u16(byte_order, out, 1); // ButtonClass
    x11::write_u16(byte_order, out, (byte_len / 4) as u16);
    x11::write_u16(byte_order, out, source_id);
    x11::write_u16(byte_order, out, num_buttons);
    out.extend(std::iter::repeat_n(0u8, 4 * state_words));
    for atom in labels {
        x11::write_u32(byte_order, out, atom.0);
    }
}

#[allow(clippy::too_many_arguments)]
fn write_valuator_class(
    byte_order: ClientByteOrder,
    out: &mut Vec<u8>,
    source_id: u16,
    number: u16,
    label: AtomId,
    min: i32,
    max: i32,
    mode: u8,
    value: i32,
) {
    x11::write_u16(byte_order, out, 2); // ValuatorClass
    x11::write_u16(byte_order, out, 11);
    x11::write_u16(byte_order, out, source_id);
    x11::write_u16(byte_order, out, number);
    x11::write_u32(byte_order, out, label.0);
    x11::write_u32(byte_order, out, min as u32);
    x11::write_u32(byte_order, out, 0);
    x11::write_u32(byte_order, out, max as u32);
    x11::write_u32(byte_order, out, 0);
    x11::write_u32(byte_order, out, value as u32);
    x11::write_u32(byte_order, out, 0);
    x11::write_u32(byte_order, out, 0); // resolution
    out.push(mode);
    out.extend_from_slice(&[0; 3]);
}

fn write_scroll_class(
    byte_order: ClientByteOrder,
    out: &mut Vec<u8>,
    source_id: u16,
    number: u16,
    scroll_type: u16,
) {
    x11::write_u16(byte_order, out, 3); // ScrollClass
    x11::write_u16(byte_order, out, 6);
    x11::write_u16(byte_order, out, source_id);
    x11::write_u16(byte_order, out, number);
    x11::write_u16(byte_order, out, scroll_type);
    x11::write_u16(byte_order, out, 0);
    // Keep the existing no-flags declaration and 1.0 increment. Yserver
    // emits smooth valuators and the corresponding emulated button events.
    x11::write_u32(byte_order, out, 0);
    x11::write_u32(byte_order, out, 1);
    x11::write_u32(byte_order, out, 0);
}

pub(crate) fn build_key_classes(byte_order: ClientByteOrder, source_id: u16) -> (Vec<u8>, u16) {
    const NUM_KEYCODES: u16 = 248;
    let mut classes = Vec::with_capacity(8 + 4 * usize::from(NUM_KEYCODES));
    x11::write_u16(byte_order, &mut classes, 0); // KeyClass
    x11::write_u16(byte_order, &mut classes, 2 + NUM_KEYCODES);
    x11::write_u16(byte_order, &mut classes, source_id);
    x11::write_u16(byte_order, &mut classes, NUM_KEYCODES);
    for keycode in 8u32..=255 {
        x11::write_u32(byte_order, &mut classes, keycode);
    }
    (classes, 1)
}
