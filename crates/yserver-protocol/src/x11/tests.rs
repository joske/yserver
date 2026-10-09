use super::*;

#[test]
fn ge_query_version_reply_uses_the_client_byte_order() {
    // Xvfb 21.1.24, GE QueryVersion(1, 0), sequence 2:
    //   LE 01000200 00000000 01000000 00…
    //   BE 01000002 00000000 00010000 00…
    for (order, expected_head) in [
        (
            ClientByteOrder::LittleEndian,
            [1u8, 0, 2, 0, 0, 0, 0, 0, 1, 0, 0, 0],
        ),
        (
            ClientByteOrder::BigEndian,
            [1u8, 0, 0, 2, 0, 0, 0, 0, 0, 1, 0, 0],
        ),
    ] {
        let mut reply = Vec::new();
        write_ge_query_version_reply(&mut reply, order, SequenceNumber(2)).unwrap();
        assert_eq!(reply.len(), 32);
        assert_eq!(&reply[..12], &expected_head, "{order:?}");
        assert!(reply[12..].iter().all(|&b| b == 0), "{order:?}");
    }
}

fn create_window_body(value_mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(28 + values.len() * 4);
    body.extend_from_slice(&0x0080_0001u32.to_le_bytes()); // wid
    body.extend_from_slice(&1u32.to_le_bytes()); // parent
    body.extend_from_slice(&0i16.to_le_bytes()); // x
    body.extend_from_slice(&0i16.to_le_bytes()); // y
    body.extend_from_slice(&10u16.to_le_bytes()); // width
    body.extend_from_slice(&10u16.to_le_bytes()); // height
    body.extend_from_slice(&0u16.to_le_bytes()); // border_width
    body.extend_from_slice(&1u16.to_le_bytes()); // class
    body.extend_from_slice(&0u32.to_le_bytes()); // visual
    body.extend_from_slice(&value_mask.to_le_bytes());
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

/// CWBorderPixmap (bit 2) and CWBorderPixel (bit 3) parse into the
/// typed fields; bit-indexed decoding keeps later attributes aligned
/// when the border bits are absent (issue #133: both were dropped).
#[test]
fn create_window_request_parses_border_attributes() {
    // Border pixel only (the awesome depth-32 pattern).
    let req = create_window_request(32, &create_window_body(0x8, &[0xff00_0000])).expect("parses");
    assert_eq!(req.border_pixmap, None);
    assert_eq!(req.border_pixel, Some(0xff00_0000));

    // Border pixmap only; 0 = CopyFromParent, kept raw for the handler.
    let req = create_window_request(24, &create_window_body(0x4, &[0x0040_0042])).expect("parses");
    assert_eq!(req.border_pixmap, Some(ResourceId(0x0040_0042)));
    assert_eq!(req.border_pixel, None);
    let req = create_window_request(24, &create_window_body(0x4, &[0])).expect("parses");
    assert_eq!(req.border_pixmap, Some(ResourceId(0)));

    // Both bits: pixmap first, then pixel — and a later attribute
    // (bit gravity) still lands in its own slot.
    let req = create_window_request(
        24,
        &create_window_body(0x4 | 0x8 | 0x10, &[0x0040_0042, 0x00ff_0000, 7]),
    )
    .expect("parses");
    assert_eq!(req.border_pixmap, Some(ResourceId(0x0040_0042)));
    assert_eq!(req.border_pixel, Some(0x00ff_0000));
    assert_eq!(req.bit_gravity, Some(7));

    // No border bits: fields are None and bit 4 is not shifted.
    let req = create_window_request(24, &create_window_body(0x10, &[7])).expect("parses");
    assert_eq!(req.border_pixmap, None);
    assert_eq!(req.border_pixel, None);
    assert_eq!(req.bit_gravity, Some(7));
}

#[test]
fn change_window_attributes_request_parses_border_attributes() {
    let body = |value_mask: u32, values: &[u32]| {
        let mut body = Vec::with_capacity(8 + values.len() * 4);
        body.extend_from_slice(&0x0080_0001u32.to_le_bytes()); // window
        body.extend_from_slice(&value_mask.to_le_bytes());
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body
    };

    let req = change_window_attributes_request(&body(0x8, &[0xff00_0000])).expect("parses");
    assert_eq!(req.border_pixmap, None);
    assert_eq!(req.border_pixel, Some(0xff00_0000));

    let req = change_window_attributes_request(&body(0x4, &[0x0040_0042])).expect("parses");
    assert_eq!(req.border_pixmap, Some(ResourceId(0x0040_0042)));
    assert_eq!(req.border_pixel, None);

    // Both bits plus cursor (bit 14): cursor stays aligned.
    let req = change_window_attributes_request(&body(
        0x4 | 0x8 | 0x4000,
        &[0x0040_0042, 0x00ff_0000, 0x0050_0001],
    ))
    .expect("parses");
    assert_eq!(req.border_pixmap, Some(ResourceId(0x0040_0042)));
    assert_eq!(req.border_pixel, Some(0x00ff_0000));
    assert_eq!(req.cursor, Some(ResourceId(0x0050_0001)));
}

/// XI2 key events carry the active keyboard group in the
/// `xXIGroupInfo` quartet (4×CARD8: base, latched, locked,
/// effective). It is derived from `state` bits 13-14. After a
/// group lock, `locked == effective == group`, `base == latched
/// == 0` (matches Xorg cinnamon-xorg.xtrace:37115).
///
/// The quartet sits immediately after the 16-byte mods quartet,
/// at a fixed offset of 76 from the event start (GenericEvent
/// header + fixed fields, see `encode_xi2_device_event`). The
/// quartet must remain exactly 4 bytes — widening it would shift
/// the trailing buttons mask and corrupt every XI2 event.
#[test]
fn xi2_device_event_encodes_group_from_state() {
    const GROUP_OFFSET: usize = 76;

    let mut buf = Vec::new();
    encode_xi2_device_event(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(1),
        131, // major
        2,   // evtype: KeyPress
        3,   // deviceid
        0,   // time
        ResourceId(0x100),
        ResourceId(0x100),
        ResourceId(0),
        0,
        0,
        0,
        0,
        0x2000, // state with group 1
        29,     // detail keycode
        3,      // sourceid
        0,      // flags
    );
    // group quartet: base=0, latched=0, locked=1, effective=1
    assert_eq!(
        &buf[GROUP_OFFSET..GROUP_OFFSET + 4],
        &[0, 0, 1, 1],
        "xXIGroupInfo quartet for state group 1"
    );

    // A group-0 call must produce an identical byte count (no drift).
    let mut buf0 = Vec::new();
    encode_xi2_device_event(
        &mut buf0,
        ClientByteOrder::LittleEndian,
        SequenceNumber(1),
        131,
        2,
        3,
        0,
        ResourceId(0x100),
        ResourceId(0x100),
        ResourceId(0),
        0,
        0,
        0,
        0,
        0x0000, // state, group 0
        29,
        3,
        0,
    );
    assert_eq!(
        buf.len(),
        buf0.len(),
        "group bits must not change byte count"
    );
    assert_eq!(
        &buf0[GROUP_OFFSET..GROUP_OFFSET + 4],
        &[0, 0, 0, 0],
        "xXIGroupInfo quartet for state group 0"
    );
}

#[test]
fn xkb_new_keyboard_notify_wire_layout() {
    let mut buf = Vec::new();
    write_xkb_new_keyboard_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
        85, // xkb_event_base
        1,  // device_id
        8,
        255, // min/max keycode (new)
        8,
        255,    // old min/max keycode
        0,      // requestMajor
        0,      // requestMinor
        0x0001, // changed = XkbNKN_KeycodesMask
    )
    .unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 85, "type = xkb_event_base + XkbEventCode(0)");
    assert_eq!(buf[1], 0, "xkbType = XkbNewKeyboardNotify");
    assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes(), "sequenceNumber @2");
    assert_eq!(buf[8], 1, "deviceID @8");
    assert_eq!(buf[9], 1, "oldDeviceID @9");
    assert_eq!(buf[10], 8, "minKeyCode @10");
    assert_eq!(buf[11], 255, "maxKeyCode @11");
    assert_eq!(buf[12], 8, "oldMinKeyCode @12");
    assert_eq!(buf[13], 255, "oldMaxKeyCode @13");
    assert_eq!(buf[14], 0, "requestMajor @14");
    assert_eq!(buf[15], 0, "requestMinor @15");
    assert_eq!(
        &buf[16..18],
        &1u16.to_le_bytes(),
        "changed = XkbNKN_KeycodesMask @16"
    );
}

#[test]
fn xkb_new_keyboard_notify_get_kbd_by_name_params() {
    // GetKbdByName path: requestMajor = the server's XKB major opcode
    // (yserver = 136; the captured Xorg trace shows its own 135),
    // requestMinor=23 (X_kbGetKbdByName), changed=0x0003 (Keycodes|Geometry).
    let mut buf = Vec::new();
    write_xkb_new_keyboard_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
        85,
        1,
        8,
        255,
        8,
        255,
        136,
        23,
        0x0003,
    )
    .unwrap();
    assert_eq!(buf[14], 136, "requestMajor = yserver XKB major opcode @14");
    assert_eq!(buf[15], 23, "requestMinor = X_kbGetKbdByName @15");
    assert_eq!(
        &buf[16..18],
        &0x0003u16.to_le_bytes(),
        "changed = Keycodes|Geometry @16"
    );
}

#[test]
fn xkb_map_notify_wire_layout() {
    let mut buf = Vec::new();
    write_xkb_map_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
        85, // xkb_event_base
        XkbMapNotify::whole_keymap(1, 8, 255, 4),
    )
    .unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 85);
    assert_eq!(buf[1], 1, "xkbType = XkbMapNotify");
    assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes(), "sequenceNumber @2");
    assert_eq!(buf[8], 1, "deviceID @8");
    // changed @10 = KeyTypes(0x01)|KeySyms(0x02)|ModifierMap(0x04) = 0x07.
    // VirtualMods(0x40) NOT claimed (vmod bindings are layout-independent;
    // virtualMods field stays 0 — every advertised bit has populated fields).
    assert_eq!(&buf[10..12], &0x0007u16.to_le_bytes(), "changed @10");
    assert_eq!(buf[12], 8, "minKeyCode @12");
    assert_eq!(buf[13], 255, "maxKeyCode @13");
    assert_eq!(buf[15], 4, "nTypes @15");
    assert_eq!(buf[16], 8, "firstKeySym @16");
    assert_eq!(buf[17], 248, "nKeySyms @17 = 255-8+1");
    assert_eq!(buf[24], 8, "firstModMapKey @24");
    assert_eq!(buf[25], 248, "nModMapKeys @25");
    assert_eq!(
        &buf[28..30],
        &0u16.to_le_bytes(),
        "virtualMods @28 = 0 (not claimed)"
    );
}

/// Golden (Xvfb 21.1.24, `xorg-xkb-change-keyboard-mapping.txt`
/// protected-one-level): the ControlsNotify for a per-key repeat change,
/// byte for byte except seq/time.
#[test]
fn xkb_controls_notify_wire_layout() {
    let mut buf = Vec::new();
    write_xkb_controls_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(9),
        0x55,
        XkbControlsNotify {
            device_id: 3,
            num_groups: 1,
            changed_controls: 0x4000_0000,
            enabled_controls: 0x0000_13a1,
            enabled_control_changes: 0,
            keycode: 0,
            event_type: 0,
            request_major: 100,
            request_minor: 0,
        },
    )
    .unwrap();
    let xorg = "550309005b8574060301000000000040a1130000000000000000640000000000";
    let xorg: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&xorg[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..], xorg[8..]);
}

/// Golden (Xvfb 21.1.24, `xorg-xkb-set-modifier-mapping.txt`
/// vmod-remap-numlock): the IndicatorMapNotify of the Num Lock map, byte
/// for byte except seq/time.
#[test]
fn xkb_indicator_map_notify_wire_layout() {
    let mut buf = Vec::new();
    write_xkb_indicator_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(9),
        0x55,
        XkbIndicatorNotify {
            kind: XkbIndicatorNotifyKind::Map,
            device_id: 3,
            state: 0,
            changed: 0x0000_0002,
        },
    )
    .unwrap();
    let xorg = "550509002397a006030000000000000002000000000000000000000000000000";
    let xorg: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&xorg[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..], xorg[8..]);
}

fn xorg_event(hex: &str) -> Vec<u8> {
    (0..32)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Golden (Xvfb 21.1.24, `xorg-xkb-setcompat.txt` si-skip-broken): the
/// CompatMapNotify of a SetCompatMap with interprets 0+3 leaving 123,
/// byte for byte except seq/time and bytes 16.. (uninitialised stack in
/// Xorg; zero here).
#[test]
fn xkb_compat_map_notify_wire_layout() {
    let mut buf = Vec::new();
    write_xkb_compat_map_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x61),
        0x55,
        XkbCompatMapNotify {
            device_id: 3,
            changed_groups: 0,
            first_si: 0,
            n_si: 3,
            n_total_si: 123,
        },
    )
    .unwrap();
    let xorg = xorg_event("550761002eae3d080300000003007b00904ffd6d90550000c013fc6d90550000");
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..16], xorg[8..16]);
    assert_eq!(buf[16..], [0; 16]);
    // xkbcomp's upload: changedGroups 0x0f, 0+124 of 124.
    buf.clear();
    write_xkb_compat_map_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x7c),
        0x55,
        XkbCompatMapNotify {
            device_id: 3,
            changed_groups: 0x0f,
            first_si: 0,
            n_si: 124,
            n_total_si: 124,
        },
    )
    .unwrap();
    let xorg = xorg_event("55077c00faab6807030f00007c007c00e09c3272425600001800000000000000");
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..16], xorg[8..16]);
}

/// Golden (Xvfb 21.1.24, `xorg-xkb-setcompat.txt` indmap-lights): the
/// ExtensionDeviceNotify of a SetIndicatorMap whose new map lit
/// indicator 20 (reason IndicatorMaps|IndicatorState), byte for byte
/// except seq/time; and the maps-only one of xkbcomp's upload.
#[test]
fn xkb_extension_device_notify_wire_layout() {
    let n = XkbExtensionDeviceNotify {
        device_id: 3,
        reason: 0x0018,
        led_class: 0,
        led_id: 0,
        leds_defined: 0x0010_3fff,
        led_state: 0x0010_0000,
        first_btn: 0,
        n_btns: 0,
        supported: 0x001f,
        unsupported: 0,
    };
    let mut buf = Vec::new();
    write_xkb_extension_device_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x61),
        0x55,
        n,
    )
    .unwrap();
    let xorg = xorg_event("550b6100b4b03d080300180000000000ff3f10000000100000001f0000000000");
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..], xorg[8..]);
    buf.clear();
    write_xkb_extension_device_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x73),
        0x55,
        XkbExtensionDeviceNotify {
            reason: 0x0008,
            leds_defined: 0x3fff,
            led_state: 0,
            ..n
        },
    )
    .unwrap();
    let xorg = xorg_event("550b7300f4ab68070300080000000000ff3f00000000000000001f0000000000");
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..], xorg[8..]);
}

/// Golden (Xvfb 21.1.24, `xorg-xkbcomp-steps.txt` identity and usru
/// step 4): the NamesNotify of xkbcomp's SetNames, byte for byte except
/// seq/time (Xorg memsets it, so its pads are zero).
#[test]
fn xkb_names_notify_wire_layout() {
    let n = XkbNamesNotify {
        device_id: 3,
        changed: 0x1fff,
        first_type: 4,
        n_types: 23,
        first_level_name: 0,
        n_level_names: 23,
        n_radio_groups: 0,
        n_aliases: 73,
        changed_group_names: 0,
        changed_virtual_mods: 0x0001,
        first_key: 8,
        n_keys: 248,
        changed_indicators: 0x3fff,
    };
    let mut buf = Vec::new();
    write_xkb_names_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x85),
        0x55,
        n,
    )
    .unwrap();
    let xorg = xorg_event("5506850001ac68070300ff1f0417001700004900010008f8ff3f000000000000");
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..], xorg[8..]);
    buf.clear();
    write_xkb_names_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x85),
        0x55,
        XkbNamesNotify {
            changed_virtual_mods: 0x0003,
            ..n
        },
    )
    .unwrap();
    let xorg = xorg_event("5506850081bb68070300ff1f0417001700004900030008f8ff3f000000000000");
    assert_eq!(buf[..4], xorg[..4]);
    assert_eq!(buf[8..], xorg[8..]);
}

/// Every `XkbNamesNotify` field at its offset, distinct values,
/// big-endian client.
#[test]
fn xkb_names_notify_field_offsets_big_endian() {
    let mut buf = Vec::new();
    write_xkb_names_notify(
        &mut buf,
        ClientByteOrder::BigEndian,
        SequenceNumber(0x0102),
        0x55,
        XkbNamesNotify {
            device_id: 3,
            changed: 0x0405,
            first_type: 6,
            n_types: 7,
            first_level_name: 8,
            n_level_names: 9,
            n_radio_groups: 0x0a,
            n_aliases: 0x0b,
            changed_group_names: 0x0c,
            changed_virtual_mods: 0x0d0e,
            first_key: 0x0f,
            n_keys: 0x10,
            changed_indicators: 0x1112_1314,
        },
    )
    .unwrap();
    assert_eq!(
        buf,
        [
            0x55, 6, 1, 2, 0, 0, 0, 0, 3, 0, 4, 5, 6, 7, 8, 9, 0, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0, 0, 0, 0
        ]
    );
}

/// Every `XkbExtensionDeviceNotify` field at its offset, distinct
/// values, big-endian client.
#[test]
fn xkb_extension_device_notify_field_offsets_big_endian() {
    let mut buf = Vec::new();
    write_xkb_extension_device_notify(
        &mut buf,
        ClientByteOrder::BigEndian,
        SequenceNumber(0x0102),
        0x55,
        XkbExtensionDeviceNotify {
            device_id: 3,
            reason: 0x0405,
            led_class: 0x0607,
            led_id: 0x0809,
            leds_defined: 0x0a0b_0c0d,
            led_state: 0x0e0f_1011,
            first_btn: 0x12,
            n_btns: 0x13,
            supported: 0x1415,
            unsupported: 0x1617,
        },
    )
    .unwrap();
    assert_eq!(
        buf,
        [
            0x55, 11, 1, 2, 0, 0, 0, 0, 3, 0, 4, 5, 6, 7, 8, 9, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0, 0
        ]
    );
}

/// Every `XkbMapNotify` field at its `xkbMapNotify` offset (XKBproto.h),
/// with distinct values so a swapped pair shows.
#[test]
fn xkb_map_notify_field_offsets() {
    let n = XkbMapNotify {
        device_id: 3,
        ptr_btn_actions: 4,
        changed: 0x0102,
        min_keycode: 8,
        max_keycode: 250,
        first_type: 5,
        n_types: 6,
        first_key_sym: 20,
        n_key_syms: 21,
        first_key_act: 22,
        n_key_acts: 23,
        first_key_behavior: 24,
        n_key_behaviors: 25,
        first_key_explicit: 26,
        n_key_explicit: 27,
        first_mod_map_key: 28,
        n_mod_map_keys: 29,
        first_vmod_map_key: 30,
        n_vmod_map_keys: 31,
        virtual_mods: 0x0a0b,
    };
    let mut le = Vec::new();
    write_xkb_map_notify(
        &mut le,
        ClientByteOrder::LittleEndian,
        SequenceNumber(7),
        90,
        n,
    )
    .unwrap();
    assert_eq!(
        le,
        [
            90, 1, 7, 0, 0, 0, 0, 0, 3, 4, 0x02, 0x01, 8, 250, 5, 6, 20, 21, 22, 23, 24, 25, 26,
            27, 28, 29, 30, 31, 0x0b, 0x0a, 0, 0
        ]
    );
    let mut be = Vec::new();
    write_xkb_map_notify(
        &mut be,
        ClientByteOrder::BigEndian,
        SequenceNumber(7),
        90,
        n,
    )
    .unwrap();
    assert_eq!(&be[2..4], &[0, 7], "sequenceNumber big-endian");
    assert_eq!(&be[10..12], &[0x01, 0x02], "changed big-endian");
    assert_eq!(&be[28..30], &[0x0a, 0x0b], "virtualMods big-endian");
}

#[test]
fn xkb_state_notify_wire_layout() {
    let mut buf = Vec::new();
    write_xkb_state_notify(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
        85, // xkb_event_base
        XkbStateNotify {
            device_id: 1,
            group: 1,
            locked_group: 1,
            mods: 0x40, // Mod4 effective
            base_mods: 0x40,
            changed: 0x0090, // XkbGroupStateMask|XkbGroupLockMask
            request_major: 136,
            request_minor: 5, // LatchLockState
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 85, "type = xkb_event_base");
    assert_eq!(buf[1], 2, "xkbType = XkbStateNotify");
    assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes(), "sequenceNumber @2");
    assert_eq!(buf[8], 1, "deviceID @8");
    assert_eq!(buf[9], 0x40, "mods @9 (effective)");
    assert_eq!(buf[19], 0x40, "compatState @19 mirrors effective mods");
    assert_eq!(buf[13], 1, "group @13");
    assert_eq!(buf[18], 1, "lockedGroup @18");
    assert_eq!(&buf[26..28], &0x0090u16.to_le_bytes(), "changed @26");
    assert_eq!(buf[30], 136, "requestMajor @30");
    assert_eq!(buf[31], 5, "requestMinor @31");
}

#[test]
fn xi_barrier_event_layout() {
    let mut buf = Vec::new();
    write_xi_barrier_event(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(3),
        137,
        25,
        2,
        1234,
        7,
        0x01,
        0x10,
        0x55,
        0,
        0,
        2,
        99,
        50,
        20.0,
        0.0,
    )
    .unwrap();
    assert_eq!(buf.len(), 68);
    assert_eq!(buf[0], 35, "GenericEvent");
    assert_eq!(buf[1], 137, "extension");
    assert_eq!(&buf[4..8], &9u32.to_le_bytes(), "length");
    assert_eq!(&buf[8..10], &25u16.to_le_bytes(), "evtype");
    assert_eq!(&buf[10..12], &2u16.to_le_bytes(), "deviceid");
    assert_eq!(&buf[16..20], &7u32.to_le_bytes(), "eventid");
    assert_eq!(&buf[28..32], &0x55u32.to_le_bytes(), "barrier");
    assert_eq!(&buf[44..48], &(99i32 << 16).to_le_bytes(), "root_x");
    assert_eq!(&buf[52..56], &20i32.to_le_bytes(), "dx integral");
    assert_eq!(&buf[56..60], &0u32.to_le_bytes(), "dx frac");
}

#[test]
fn parse_xi_barrier_release_entries() {
    let mut body = Vec::new();
    body.extend_from_slice(&2u32.to_le_bytes());
    for (dev, bar, eid) in [(2u16, 0x55u32, 7u32), (2, 0x66, 9)] {
        body.extend_from_slice(&dev.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&bar.to_le_bytes());
        body.extend_from_slice(&eid.to_le_bytes());
    }
    let entries = parse_xi_barrier_release(&body).expect("parse");
    assert_eq!(entries, vec![(2, 0x55, 7), (2, 0x66, 9)]);
}

/// XI1 `GetDeviceKeyMapping` reply wire layout, asserted against the
/// `xGetDeviceKeyMappingReply` struct in XIproto.h:
///   byte 0 = X_Reply (1); byte 1 = X_GetDeviceKeyMapping (24);
///   bytes 2..4 = sequence; bytes 4..8 = length (= count*kpc words);
///   byte 8 = keySymsPerKeyCode; bytes 9..32 = pad; then keysyms.
#[test]
fn get_device_key_mapping_reply_matches_xiproto_layout() {
    // 2 keycodes, keysyms-per-keycode = 3 → 6 keysym words.
    let keysyms: [u32; 6] = [0x61, 0x41, 0, 0x62, 0x42, 0];
    let mut buf = Vec::new();
    write_get_device_key_mapping_reply(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
        3,
        &keysyms,
    )
    .unwrap();

    assert_eq!(buf.len(), 32 + 6 * 4);
    assert_eq!(buf[0], 1, "repType = X_Reply");
    assert_eq!(buf[1], 24, "RepType = X_GetDeviceKeyMapping");
    assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes(), "sequenceNumber");
    assert_eq!(&buf[4..8], &6u32.to_le_bytes(), "length = count*kpc words");
    assert_eq!(buf[8], 3, "keySymsPerKeyCode");
    assert_eq!(&buf[9..32], &[0u8; 23], "pad0..pad6");
    for (i, k) in keysyms.iter().enumerate() {
        let off = 32 + i * 4;
        assert_eq!(&buf[off..off + 4], &k.to_le_bytes(), "keysym {i}");
    }
}

/// XI1 `GetFeedbackControl` keyboard reply, asserted against
/// `xGetFeedbackControlReply` + `xKbdFeedbackState` (XIproto.h).
#[test]
fn get_feedback_control_kbd_reply_layout() {
    let auto_repeats = [0xAAu8; 32];
    let kbd = encode_kbd_feedback_state(
        ClientByteOrder::LittleEndian,
        0,
        0x1234, // pitch
        0x5678, // duration
        0x0000_000F,
        true,
        0x11, // click
        0x22, // percent
        &auto_repeats,
    );
    assert_eq!(kbd.len(), 52);
    assert_eq!(kbd[0], 0, "KbdFeedbackClass");
    assert_eq!(kbd[1], 0, "id");
    assert_eq!(&kbd[2..4], &52u16.to_le_bytes(), "length = 52");
    assert_eq!(&kbd[4..6], &0x1234u16.to_le_bytes(), "pitch");
    assert_eq!(&kbd[6..8], &0x5678u16.to_le_bytes(), "duration");
    assert_eq!(&kbd[8..12], &0x0000_000Fu32.to_le_bytes(), "led_mask");
    assert_eq!(
        &kbd[12..16],
        &0x0000_000Fu32.to_le_bytes(),
        "led_values=led_mask"
    );
    assert_eq!(kbd[16], 1, "global_auto_repeat");
    assert_eq!(kbd[17], 0x11, "click");
    assert_eq!(kbd[18], 0x22, "percent");
    assert_eq!(kbd[19], 0, "pad");
    assert_eq!(&kbd[20..52], &auto_repeats, "auto_repeats[32]");

    let mut buf = Vec::new();
    write_get_feedback_control_reply(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(9),
        1,
        &kbd,
    )
    .unwrap();
    assert_eq!(buf.len(), 32 + 52);
    assert_eq!(buf[0], 1, "X_Reply");
    assert_eq!(buf[1], 22, "RepType = X_GetFeedbackControl");
    assert_eq!(&buf[4..8], &13u32.to_le_bytes(), "length = 52/4 words");
    assert_eq!(&buf[8..10], &1u16.to_le_bytes(), "num_feedbacks");
    assert_eq!(&buf[32..], &kbd[..], "feedback payload");
}

/// XI1 `GetFeedbackControl` pointer reply (`xPtrFeedbackState`).
#[test]
fn get_feedback_control_ptr_state_layout() {
    let ptr = encode_ptr_feedback_state(ClientByteOrder::LittleEndian, 0, 2, 1, 4);
    assert_eq!(ptr.len(), 12);
    assert_eq!(ptr[0], 1, "PtrFeedbackClass");
    assert_eq!(ptr[1], 0, "id");
    assert_eq!(&ptr[2..4], &12u16.to_le_bytes(), "length = 12");
    assert_eq!(&ptr[4..6], &[0u8, 0], "pad1, pad2");
    assert_eq!(&ptr[6..8], &2u16.to_le_bytes(), "accelNum");
    assert_eq!(&ptr[8..10], &1u16.to_le_bytes(), "accelDenom");
    assert_eq!(&ptr[10..12], &4u16.to_le_bytes(), "threshold");
}

/// XI1 `GetDeviceMotionEvents` empty-history reply, asserted against
/// `xGetDeviceMotionEventsReply` in XIproto.h: RepType=10@1,
/// nEvents=0@8, axes@12, mode@13, length=0.
#[test]
fn get_device_motion_events_reply_empty_history_layout() {
    let mut buf = Vec::new();
    write_get_device_motion_events_reply(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x55),
        4,
        1,
    )
    .unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 1, "repType = X_Reply");
    assert_eq!(buf[1], 10, "RepType = X_GetDeviceMotionEvents");
    assert_eq!(&buf[2..4], &0x55u16.to_le_bytes(), "sequenceNumber");
    assert_eq!(&buf[4..8], &0u32.to_le_bytes(), "length = 0 (no history)");
    assert_eq!(&buf[8..12], &0u32.to_le_bytes(), "nEvents = 0");
    assert_eq!(buf[12], 4, "axes");
    assert_eq!(buf[13], 1, "mode = Absolute");
    assert_eq!(&buf[14..32], &[0u8; 18], "pads");
}

/// Empty range (count=0) → header only, length 0, still 32 bytes.
#[test]
fn get_device_key_mapping_reply_empty_is_header_only() {
    let mut buf = Vec::new();
    write_get_device_key_mapping_reply(
        &mut buf,
        ClientByteOrder::BigEndian,
        SequenceNumber(7),
        4,
        &[],
    )
    .unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 1);
    assert_eq!(buf[1], 24);
    assert_eq!(&buf[2..4], &7u16.to_be_bytes());
    assert_eq!(&buf[4..8], &0u32.to_be_bytes());
    assert_eq!(buf[8], 4);
}

/// The fallback GetImage reply must keep its length field and
/// payload consistent with the requested format + plane_mask:
/// XYPixmap carries popcount(mask) bitmap planes, and an empty
/// mask yields a 0-length reply (libX11 NULL-derefs otherwise).
#[test]
fn get_image_fallback_reply_sizes_match_format() {
    let req = |format: u8, plane_mask: u32| GetImageRequest {
        format,
        drawable: ResourceId(1),
        x: 0,
        y: 0,
        width: 40,
        height: 2,
        plane_mask,
    };
    let write = |r: &GetImageRequest| {
        let mut buf = Vec::new();
        write_get_image_reply(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            r,
            0x21,
        )
        .unwrap();
        buf
    };

    // XYPixmap, plane_mask=0 → header only, length 0.
    let buf = write(&req(1, 0));
    assert_eq!(buf.len(), 32);
    assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 0);

    // XYPixmap, 3 planes → 3 × stride(40px→8B) × 2 rows = 48 bytes.
    let buf = write(&req(1, 0b111));
    assert_eq!(buf.len(), 32 + 48);
    assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 12);

    // ZPixmap → full 32bpp grid regardless of mask.
    let buf = write(&req(2, 0));
    assert_eq!(buf.len(), 32 + 40 * 2 * 4);
}

#[test]
fn encode_shm_completion_event_wire_layout() {
    // Ground-truthed against `xShmCompletionEvent` (shmproto.h):
    // type, bpad, seq, drawable, minorEvent, majorEvent, bpad,
    // shmseg, offset, 12 pad — 32 bytes total.
    let mut buf = Vec::new();
    encode_shm_completion_event(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
        65, // event_code = first_event + ShmCompletion(0)
        ResourceId(0xDEAD_BEEF),
        3,           // X_ShmPutImage
        130,         // MIT-SHM major
        0x00AB_CDEF, // shmseg
        0x0001_0000, // offset
    );
    assert_eq!(buf.len(), 32, "ShmCompletion is 32 bytes");
    assert_eq!(buf[0], 65, "event code");
    assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234, "sequence");
    assert_eq!(
        u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        0xDEAD_BEEF,
        "drawable"
    );
    assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 3, "minorEvent");
    assert_eq!(buf[10], 130, "majorEvent");
    assert_eq!(
        u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        0x00AB_CDEF,
        "shmseg"
    );
    assert_eq!(
        u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        0x0001_0000,
        "offset"
    );
}

#[test]
fn encode_configure_notify_event_writes_above_sibling() {
    let mut buf = Vec::new();
    encode_configure_notify_event(
        &mut buf,
        SequenceNumber(0x1234),
        ClientByteOrder::LittleEndian,
        ResourceId(0x100),
        ResourceId(0x200),
        Some(ResourceId(0x300)),
        Geometry {
            root: ResourceId(0x100),
            x: 10,
            y: 20,
            width: 640,
            height: 480,
            border_width: 2,
            depth: 24,
        },
        false,
    );

    assert_eq!(buf[0], 22);
    assert_eq!(u32::from_le_bytes(buf[12..16].try_into().unwrap()), 0x300);
}

#[test]
fn write_list_fonts_with_info_reply_round_trip() {
    // Matches the byte layout libXt's xcb_list_fonts_with_info_reply_t
    // expects. We assert each field's offset explicitly because LFWI
    // sits in the same wire-format family as QueryFont but with a
    // name+padding tail instead of charinfo data — easy to get the
    // tail length wrong.
    let metrics = FontMetrics {
        min_bounds: CharInfo {
            left_side_bearing: 0,
            right_side_bearing: 6,
            character_width: 6,
            ascent: 12,
            descent: 4,
            attributes: 0,
        },
        max_bounds: CharInfo {
            left_side_bearing: 0,
            right_side_bearing: 7,
            character_width: 7,
            ascent: 13,
            descent: 4,
            attributes: 0,
        },
        min_char_or_byte2: 32,
        max_char_or_byte2: 126,
        default_char: 32,
        draw_direction: 0,
        min_byte1: 0,
        max_byte1: 0,
        all_chars_exist: true,
        font_ascent: 13,
        font_descent: 4,
        properties: Vec::new(),
        named_properties: Vec::new(),
        char_infos: Vec::new(),
    };
    let name = "-fc-dejavu sans mono-medium-r-normal--12-120-75-75-m-72-iso8859-1";
    let mut buf = Vec::new();
    write_list_fonts_with_info_reply(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0xabcd),
        &metrics,
        name,
        7,
    )
    .unwrap();

    assert_eq!(buf[0], 1, "reply type");
    assert_eq!(buf[1] as usize, name.len(), "name_len");
    // sequence at [2..4]
    assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0xabcd);
    // reply_length: word count after the 32-byte header.
    let words = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assert_eq!(words as usize * 4 + 32, buf.len());
    // min_bounds.character_width at [8+4..8+6] = [12..14]
    assert_eq!(i16::from_le_bytes([buf[12], buf[13]]), 6);
    // max_bounds.character_width at [24+4..24+6] = [28..30]
    assert_eq!(i16::from_le_bytes([buf[28], buf[29]]), 7);
    // all-chars-exist at [51]
    assert_eq!(buf[51], 1);
    // font-ascent at [52..54], font-descent at [54..56]
    assert_eq!(i16::from_le_bytes([buf[52], buf[53]]), 13);
    assert_eq!(i16::from_le_bytes([buf[54], buf[55]]), 4);
    // replies-hint at [56..60]
    assert_eq!(u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]), 7);
    // Name follows (no FONTPROPs, so it starts at offset 60).
    assert_eq!(&buf[60..60 + name.len()], name.as_bytes());
    // Trailing padding is zero.
    for &b in &buf[60 + name.len()..] {
        assert_eq!(b, 0, "padding must be zero");
    }
}

#[test]
fn write_list_fonts_with_info_terminator_layout() {
    let mut buf = Vec::new();
    write_list_fonts_with_info_terminator(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(0x1234),
    )
    .unwrap();
    assert_eq!(buf.len(), 60, "fixed 60-byte LFWI terminator");
    assert_eq!(buf[0], 1, "reply type");
    assert_eq!(buf[1], 0, "name_len = 0 → terminator");
    assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234);
    assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 7);
}

#[test]
fn read_request_rejects_zero_length_without_big_requests() {
    let mut input = std::io::Cursor::new([1, 2, 0, 0]);
    let err = read_request(&mut input, ClientByteOrder::LittleEndian, false)
        .expect_err("zero length must be invalid");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn read_request_accepts_big_requests_extended_length() {
    let mut input = std::io::Cursor::new([
        1, 2, 0, 0, // normal header with extended length marker
        3, 0, 0, 0, // 12-byte total request: header + big length + 4 body bytes
        0xaa, 0xbb, 0xcc, 0xdd,
    ]);
    let (header, body) = read_request(&mut input, ClientByteOrder::LittleEndian, true)
        .expect("read should succeed")
        .expect("request should be present");

    assert_eq!(header.opcode, 1);
    assert_eq!(header.data, 2);
    assert_eq!(header.length_units, 3);
    assert_eq!(body, [0xaa, 0xbb, 0xcc, 0xdd]);
}

#[test]
fn read_request_be_normal_length() {
    // Opcode 1, data 2, length_units = 3 in BE (00 03), body = 8 bytes.
    let mut input = std::io::Cursor::new([
        1, 2, 0x00, 0x03, // BE-encoded length = 3 units
        0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44,
    ]);
    let (header, body) = read_request(&mut input, ClientByteOrder::BigEndian, false)
        .expect("read should succeed")
        .expect("request should be present");
    assert_eq!(header.opcode, 1);
    assert_eq!(header.data, 2);
    assert_eq!(header.length_units, 3);
    assert_eq!(body.len(), 8);
    assert_eq!(body, [0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44]);
}

#[test]
fn read_request_be_big_requests_extended_length() {
    // Opcode 1, data 2, length_units = 0 (BE) → BIG-REQUESTS path,
    // big length = 3 (BE), so total = 12 bytes, body = 4 bytes.
    let mut input = std::io::Cursor::new([
        1, 2, 0x00, 0x00, // BE-encoded length = 0 → BIG path
        0x00, 0x00, 0x00, 0x03, // BE-encoded big length = 3 units
        0xaa, 0xbb, 0xcc, 0xdd,
    ]);
    let (header, body) = read_request(&mut input, ClientByteOrder::BigEndian, true)
        .expect("read should succeed")
        .expect("request should be present");
    assert_eq!(header.opcode, 1);
    assert_eq!(header.length_units, 3);
    assert_eq!(body, [0xaa, 0xbb, 0xcc, 0xdd]);
}

#[test]
fn read_request_flags_malformed_big_length_under_two_units() {
    // xts5 ListInputDevices-2 sends `length=0, big=1` to probe
    // BadLength on sub-header BIG-REQUESTS. We must NOT disconnect
    // — emit a sentinel `length_units = u32::MAX` so the
    // dispatcher's max-length gate fires BadLength.
    let mut input = std::io::Cursor::new([
        2, 0, 0, 0, // ListInputDevices opcode, length=0
        1, 0, 0, 0, // BIG length = 1 (LE) — sub-header
    ]);
    let (header, body) = read_request(&mut input, ClientByteOrder::LittleEndian, true)
        .expect("read should succeed")
        .expect("request should be present");
    assert_eq!(header.opcode, 2);
    assert_eq!(header.length_units, u32::MAX);
    assert!(body.is_empty());
}

#[test]
fn read_request_accepts_a_4mib_big_request() {
    // Chromium sends ~4 MiB PutImage requests; the old 1 MiB cap rejected them (#166).
    const UNITS: u32 = 1024 * 1024 + 8;
    const BODY_BYTES: usize = (UNITS as usize * 4) - 8;
    let mut input = Vec::with_capacity(8 + BODY_BYTES);
    input.extend_from_slice(&[72, 2, 0, 0]);
    input.extend_from_slice(&UNITS.to_le_bytes());
    input.resize(input.len() + BODY_BYTES, 0x55);
    let mut cursor = std::io::Cursor::new(input);
    let (header, body) = read_request(&mut cursor, ClientByteOrder::LittleEndian, true)
        .expect("read should succeed")
        .expect("request should be present");
    assert_eq!(header.length_units, UNITS);
    assert_eq!(body.len(), BODY_BYTES);
}

#[test]
fn big_requests_enable_reply_advertises_xorg_maximum() {
    let mut buf = Vec::new();
    write_big_requests_enable_reply(
        &mut buf,
        ClientByteOrder::LittleEndian,
        SequenceNumber(1),
        MAX_BIG_REQUEST_UNITS,
    )
    .expect("encode");
    assert_eq!(
        u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        4_194_303
    );
}

#[test]
fn read_request_flags_over_max_big_length_and_drains_body() {
    // xts5 TOO_LONG: bigRequestLength = max+1 with the
    // matching payload bytes on the wire. We override length to
    // the BadLength sentinel and drain the payload to keep the
    // socket aligned for the next request.
    const OVER_MAX: u32 = MAX_BIG_REQUEST_UNITS + 1;
    const BODY_BYTES: usize = (OVER_MAX as usize * 4) - 8;
    let mut input = Vec::with_capacity(8 + BODY_BYTES + 4);
    input.extend_from_slice(&[1, 2, 0, 0]); // opcode, data, length=0
    input.extend_from_slice(&OVER_MAX.to_le_bytes()); // BIG length
    input.resize(input.len() + BODY_BYTES, 0xaa); // claimed payload
    // Sentinel bytes after the malformed request — must be intact
    // for the next read_request call.
    input.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

    let mut cursor = std::io::Cursor::new(input);
    let (header, body) = read_request(&mut cursor, ClientByteOrder::LittleEndian, true)
        .expect("read should succeed")
        .expect("request should be present");
    assert_eq!(header.opcode, 1);
    assert_eq!(header.length_units, u32::MAX);
    assert!(body.is_empty());

    // The cursor should now be positioned at the sentinel.
    assert_eq!(cursor.position() as usize, 8 + BODY_BYTES);
    let mut next = [0u8; 4];
    std::io::Read::read_exact(&mut cursor, &mut next).unwrap();
    assert_eq!(next, [0xde, 0xad, 0xbe, 0xef]);
}

#[test]
fn write_error_be_encodes_fields_in_big_endian() {
    let mut buf = Vec::new();
    write_error(
        &mut buf,
        ClientByteOrder::BigEndian,
        SequenceNumber(0x1234),
        error::BAD_LENGTH,
        0xdead_beef,
        0x4321,
        42,
    )
    .unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 0); // error response_type
    assert_eq!(buf[1], error::BAD_LENGTH);
    // sequence is u16 BE at [2..4]
    assert_eq!(&buf[2..4], &[0x12, 0x34]);
    // bad_value is u32 BE at [4..8]
    assert_eq!(&buf[4..8], &[0xde, 0xad, 0xbe, 0xef]);
    // minor_opcode is u16 BE at [8..10]
    assert_eq!(&buf[8..10], &[0x43, 0x21]);
    // major_opcode is u8 at [10]
    assert_eq!(buf[10], 42);
}

// Wire layout per the X11 core protocol "Connection Setup" (failed):
//   1  success=0   1  lengthReason   2  major   2  minor
//   2  length=(reason+pad)/4         n  reason  p  pad
// reason="no" (n=2) → padded to 4 → length=1.

#[test]
fn write_setup_failed_little_endian_layout() {
    let mut out = Vec::new();
    write_setup_failed(&mut out, ClientByteOrder::LittleEndian, "no").unwrap();
    assert_eq!(
        out,
        vec![
            0x00, 0x02, // success=0, lengthReason=2
            0x0b, 0x00, // major=11 (LE)
            0x00, 0x00, // minor=0
            0x01, 0x00, // length=1 (LE)
            b'n', b'o', 0x00, 0x00, // reason + pad to 4
        ]
    );
}

#[test]
fn write_setup_failed_big_endian_layout() {
    let mut out = Vec::new();
    write_setup_failed(&mut out, ClientByteOrder::BigEndian, "no").unwrap();
    assert_eq!(
        out,
        vec![
            0x00, 0x02, // success=0, lengthReason=2
            0x00, 0x0b, // major=11 (BE)
            0x00, 0x00, // minor=0
            0x00, 0x01, // length=1 (BE)
            b'n', b'o', 0x00, 0x00,
        ]
    );
}

// Empty reason: lengthReason=0, length=pad4(0)/4=0, no trailing bytes —
// an 8-byte prefix and nothing more.
#[test]
fn write_setup_failed_empty_reason_is_prefix_only() {
    let mut out = Vec::new();
    write_setup_failed(&mut out, ClientByteOrder::LittleEndian, "").unwrap();
    assert_eq!(
        out,
        vec![
            0x00, 0x00, // success=0, lengthReason=0
            0x0b, 0x00, // major=11 (LE)
            0x00, 0x00, // minor=0
            0x00, 0x00, // length=0 (LE)
        ]
    );
}

#[test]
fn xi2_motion_event_carries_axes() {
    let mut out = Vec::new();
    encode_xi2_device_event(
        &mut out,
        ClientByteOrder::LittleEndian,
        SequenceNumber(7),
        137,
        6, // XI_Motion
        2,
        123,
        ResourceId(0x100),
        ResourceId(0x200),
        ResourceId(0x300),
        1,
        2,
        3,
        4,
        5,
        0, // detail = 0 for motion
        2,
        0, // flags
    );

    // Width-padded to match Xorg byte-for-byte: 32 header + 16 coords
    // + 12 lens/sourceid/pad/flags + 16 mods + 4 group +
    // 32 buttons mask (8 u32s) + 8 valuator mask (2 u32s) +
    // 16 axisvalues (2 FP3232 X/Y) = 136 bytes.
    assert_eq!(out.len(), 136);
    assert_eq!(&out[0..4], &[35, 137, 7, 0]);
    // length-in-4-byte-units beyond the 32-byte header: (136 - 32) / 4 = 26.
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 26);
    // Valuator mask at offset 112 (after group at 80 + 32 button mask): X+Y bits.
    assert_eq!(u32::from_le_bytes(out[112..116].try_into().unwrap()), 0x3);
    // X axis integer part at offset 120 (after 8 byte valuator mask): root_x = 1.
    assert_eq!(u32::from_le_bytes(out[120..124].try_into().unwrap()), 1);
    // X fraction at offset 124: 0.
    assert_eq!(u32::from_le_bytes(out[124..128].try_into().unwrap()), 0);
    // Y axis integer part at offset 128: root_y = 2.
    assert_eq!(u32::from_le_bytes(out[128..132].try_into().unwrap()), 2);
}

#[test]
fn xi2_button_event_has_no_axes() {
    // Matches the Xorg reference: XI_ButtonPress / XI_ButtonRelease
    // carry valuators_len=0, no valuator mask, no axisvalues.
    // Sampled from thunar-inside-MATE 2026-05-15.
    let mut out = Vec::new();
    encode_xi2_device_event(
        &mut out,
        ClientByteOrder::LittleEndian,
        SequenceNumber(7),
        137,
        4, // XI_ButtonPress
        2,
        123,
        ResourceId(0x100),
        ResourceId(0x200),
        ResourceId(0x300),
        1,
        2,
        3,
        4,
        5,
        1,
        2,
        0, // flags
    );

    // Width-padded to match Xorg: 32 header + 16 coords + 12 lens +
    // 16 mods + 4 group + 32 buttons mask (8 u32s) + 8 valuator
    // mask (2 u32s) + 0 axes = 120 bytes.
    assert_eq!(out.len(), 120);
    assert_eq!(&out[0..4], &[35, 137, 7, 0]);
    // length units: (120 - 32) / 4 = 22.
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 22);
    // buttons_len at offset 48: 8 (Xorg-matching pad width).
    assert_eq!(u16::from_le_bytes(out[48..50].try_into().unwrap()), 8);
    // valuators_len at offset 50: 2 (Xorg-matching pad width).
    assert_eq!(u16::from_le_bytes(out[50..52].try_into().unwrap()), 2);
    // Valuator mask at offset 112 (after 32-byte buttons mask): 0 — button
    // events change no axes.
    assert_eq!(u32::from_le_bytes(out[112..116].try_into().unwrap()), 0);
}

#[test]
fn xi2_raw_button_event_matches_xorg_empty_valuator_payload() {
    let mut out = vec![0xaa; 4];
    let start = out.len();
    encode_xi2_raw_event(
        &mut out,
        ClientByteOrder::LittleEndian,
        SequenceNumber(7),
        137,
        15,
        4,
        0x5436,
        1,
        4,
        1483,
        449,
    );

    let event = &out[start..];
    assert_eq!(event.len(), 40);
    let length = u32::from_le_bytes(event[4..8].try_into().unwrap());
    assert_eq!(length, 2);
    assert_eq!(event.len(), 32 + length as usize * 4);
    assert_eq!(u16::from_le_bytes(event[22..24].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(event[32..36].try_into().unwrap()), 0);
    assert_eq!(u32::from_le_bytes(event[36..40].try_into().unwrap()), 0);
}

#[test]
fn xi2_raw_motion_event_carries_xy_valuators() {
    let mut event = Vec::new();
    encode_xi2_raw_event(
        &mut event,
        ClientByteOrder::LittleEndian,
        SequenceNumber(7),
        137,
        17,
        4,
        0x5436,
        0,
        4,
        1483,
        449,
    );

    assert_eq!(event.len(), 72);
    assert_eq!(u32::from_le_bytes(event[4..8].try_into().unwrap()), 10);
    assert_eq!(u16::from_le_bytes(event[22..24].try_into().unwrap()), 2);
    assert_eq!(u32::from_le_bytes(event[32..36].try_into().unwrap()), 0x3);
    assert_eq!(u32::from_le_bytes(event[36..40].try_into().unwrap()), 0);
    assert_eq!(i32::from_le_bytes(event[40..44].try_into().unwrap()), 1483);
}

#[test]
fn xi2_crossing_event_has_expected_wire_size() {
    let mut out = Vec::new();
    encode_xi2_crossing_event(
        &mut out,
        ClientByteOrder::LittleEndian,
        SequenceNumber(8),
        137,
        7,
        2,
        123,
        ResourceId(0x100),
        ResourceId(0x200),
        1,
        2,
        3,
        4,
        5,
        0,
        0,
        2,
        false,
    );

    assert_eq!(out.len(), 76);
    assert_eq!(&out[0..4], &[35, 137, 8, 0]);
    assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 11);
}

mod change_property_tests {
    use super::*;
    use proptest::prelude::*;

    fn encode(req: &ChangePropertyRequest) -> (u8, Vec<u8>) {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.window.0);
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.property.0);
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.r#type.0);
        body.push(req.format);
        body.extend_from_slice(&[0; 3]);
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.length);
        body.extend_from_slice(&req.data);
        pad_vec4(&mut body);
        (req.mode, body)
    }

    proptest! {
        #[test]
        fn round_trip(
            mode in 0u8..=2,
            window in any::<u32>(),
            property in 1u32..0xFFFF,
            r#type in 1u32..0xFFFF,
            format_choice in 0u8..3,
            length in 0u32..256,
        ) {
            let format = [8u8, 16, 32][format_choice as usize];
            let unit = match format { 8 => 1, 16 => 2, _ => 4 };
            let data = vec![0xAB; (length as usize) * unit];
            let req = ChangePropertyRequest {
                mode,
                window: ResourceId(window),
                property: AtomId(property),
                r#type: AtomId(r#type),
                format,
                data: data.clone(),
                length,
            };
            let (header_data, body) = encode(&req);
            let parsed = change_property_request(header_data, &body).unwrap();
            prop_assert_eq!(parsed, req);
        }
    }

    #[test]
    fn invalid_format_passes_through_for_handler_to_reject() {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 31);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 31);
        body.push(7); // invalid format byte
        body.extend_from_slice(&[0; 3]);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0); // length = 0
        let req =
            change_property_request(0, &body).expect("parser should pass through invalid format");
        assert_eq!(req.format, 7);
        assert_eq!(req.length, 0);
    }
}

mod delete_property_tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn round_trip(window in any::<u32>(), property in any::<u32>()) {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, window);
            write_u32(ClientByteOrder::LittleEndian, &mut body, property);
            let req = delete_property_request(&body).unwrap();
            prop_assert_eq!(req, DeletePropertyRequest {
                window: ResourceId(window), property: AtomId(property),
            });
        }
    }
}

mod render_reply_tests {
    use super::*;

    #[test]
    fn query_pict_index_values_empty_reply_shape() {
        let mut out = Vec::new();
        write_render_query_pict_index_values_reply(
            &mut out,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0x1234),
        )
        .unwrap();

        assert_eq!(out.len(), 32);
        assert_eq!(out[0], 1);
        assert_eq!(out[1], 0);
        assert_eq!(&out[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&out[4..8], &0u32.to_le_bytes());
        assert_eq!(&out[8..12], &0u32.to_le_bytes());
        assert!(out[12..].iter().all(|b| *b == 0));
    }

    #[test]
    fn query_filters_advertises_standard_xorg_set() {
        // Mirrors X.Org's render.c QueryFilters encoding: three
        // canonical filters (nearest/bilinear/convolution) with
        // FilterAliasNone (0xFFFF) alias slots, then three aliases
        // (fast→nearest, good→bilinear, best→bilinear) with their
        // canonical filter's INDEX in the alias slot.
        let mut out = Vec::new();
        write_render_query_filters_reply(
            &mut out,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0x1234),
        )
        .unwrap();

        // 32-byte header + 12-byte alias array (6 × u16) + 44-byte
        // names list (5+5+5+8+9+12 names, no trailing pad needed).
        assert_eq!(out.len(), 32 + 12 + 44);
        assert_eq!(out[0], 1);
        assert_eq!(out[1], 0);
        assert_eq!(&out[2..4], &0x1234u16.to_le_bytes());
        // length_words = (12 + 44) / 4 = 14
        assert_eq!(&out[4..8], &14u32.to_le_bytes());
        // num_aliases = num_filters = 6
        assert_eq!(&out[8..12], &6u32.to_le_bytes());
        assert_eq!(&out[12..16], &6u32.to_le_bytes());
        assert!(out[16..32].iter().all(|b| *b == 0));

        // aliases[0..3] = FilterAliasNone for the canonical filters.
        assert_eq!(&out[32..34], &0xFFFFu16.to_le_bytes());
        assert_eq!(&out[34..36], &0xFFFFu16.to_le_bytes());
        assert_eq!(&out[36..38], &0xFFFFu16.to_le_bytes());
        // aliases[3..6]: fast→0 (nearest), good→1 (bilinear), best→1.
        assert_eq!(&out[38..40], &0u16.to_le_bytes());
        assert_eq!(&out[40..42], &1u16.to_le_bytes());
        assert_eq!(&out[42..44], &1u16.to_le_bytes());

        // Name list: canonical first, then aliases.
        let names = &out[44..];
        assert_eq!(names[0], 7);
        assert_eq!(&names[1..8], b"nearest");
        assert_eq!(names[8], 8);
        assert_eq!(&names[9..17], b"bilinear");
        assert_eq!(names[17], 11);
        assert_eq!(&names[18..29], b"convolution");
        assert_eq!(names[29], 4);
        assert_eq!(&names[30..34], b"fast");
        assert_eq!(names[34], 4);
        assert_eq!(&names[35..39], b"good");
        assert_eq!(names[39], 4);
        assert_eq!(&names[40..44], b"best");
    }
}

mod get_property_tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn round_trip(
            delete: bool,
            window in any::<u32>(),
            property in any::<u32>(),
            r#type in any::<u32>(),
            long_offset in any::<u32>(),
            long_length in any::<u32>(),
        ) {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, window);
            write_u32(ClientByteOrder::LittleEndian, &mut body, property);
            write_u32(ClientByteOrder::LittleEndian, &mut body, r#type);
            write_u32(ClientByteOrder::LittleEndian, &mut body, long_offset);
            write_u32(ClientByteOrder::LittleEndian, &mut body, long_length);
            let req = get_property_request(if delete { 1 } else { 0 }, &body).unwrap();
            prop_assert_eq!(req, GetPropertyRequest {
                delete,
                window: ResourceId(window),
                property: AtomId(property),
                r#type: AtomId(r#type),
                long_offset,
                long_length,
            });
        }
    }
}

mod get_property_reply_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn shape(
            format_choice in 0u8..3,
            r#type in any::<u32>(),
            bytes_after in any::<u32>(),
            len_units in 0u32..256,
        ) {
            let format = [8u8, 16, 32][format_choice as usize];
            let unit = match format { 8 => 1, 16 => 2, _ => 4 };
            let value: Vec<u8> = (0..len_units as usize * unit)
                .map(|i| (i & 0xff) as u8)
                .collect();
            let value_len = len_units;

            let mut buf = Vec::new();
            write_get_property_reply(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(0xdead),
                GetPropertyReply {
                    format,
                    r#type: AtomId(r#type),
                    bytes_after,
                    value_len,
                    value: &value,
                },
            )
            .unwrap();

            let pad = (4 - value.len() % 4) % 4;
            let payload = value.len() + pad;
            prop_assert_eq!(buf.len(), 32 + payload);
            // wire length field (4..8) equals payload/4
            let wire_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            prop_assert_eq!(wire_len as usize * 4, payload);
            // value_len field (16..20) is in format units
            let wire_value_len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
            prop_assert_eq!(wire_value_len, value_len);
        }
    }
}

mod property_notify_tests {
    use super::*;
    #[test]
    fn shape() {
        let mut buf = Vec::new();
        encode_property_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100002),
            AtomId(0x42),
            0xdead_beef,
            true,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 28);
        assert_eq!(&buf[2..4], &[0x34, 0x12]);
        assert_eq!(&buf[4..8], &0x100002u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x42u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0xdead_beefu32.to_le_bytes());
        assert_eq!(buf[16], 1);
    }
}

mod destroy_notify_tests {
    use super::*;
    #[test]
    fn shape() {
        let mut buf = Vec::new();
        encode_destroy_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100),
            ResourceId(0x100002),
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 17);
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
    }
}

mod unmap_notify_tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn shape() {
        let mut buf = Vec::new();
        encode_unmap_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100),
            ResourceId(0x100002),
            false,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 18);
        assert_eq!(buf[1], 0);
        assert_eq!(&buf[2..4], &[0x34, 0x12]);
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
        assert_eq!(buf[12], 0);
        assert!(buf[13..32].iter().all(|&b| b == 0));
    }

    proptest! {
        #[test]
        fn encoder_round_trip(
            sequence in any::<u16>(),
            event_window in any::<u32>(),
            window in any::<u32>(),
            from_configure: bool,
            big_endian: bool,
        ) {
            let order = if big_endian {
                ClientByteOrder::BigEndian
            } else {
                ClientByteOrder::LittleEndian
            };
            let mut buf = Vec::new();
            encode_unmap_notify_event(
                &mut buf,
                SequenceNumber(sequence),
                order,
                ResourceId(event_window),
                ResourceId(window),
                from_configure,
            );
            prop_assert_eq!(buf.len(), 32);
            prop_assert_eq!(buf[0], 18);
            prop_assert_eq!(buf[1], 0);

            let seq_bytes = if big_endian {
                sequence.to_be_bytes()
            } else {
                sequence.to_le_bytes()
            };
            prop_assert_eq!(&buf[2..4], &seq_bytes[..]);

            let ew_bytes = if big_endian {
                event_window.to_be_bytes()
            } else {
                event_window.to_le_bytes()
            };
            prop_assert_eq!(&buf[4..8], &ew_bytes[..]);

            let w_bytes = if big_endian {
                window.to_be_bytes()
            } else {
                window.to_le_bytes()
            };
            prop_assert_eq!(&buf[8..12], &w_bytes[..]);

            prop_assert_eq!(buf[12], u8::from(from_configure));
            prop_assert!(buf[13..32].iter().all(|&b| b == 0));
        }
    }
}

mod reparent_tests {
    use super::*;

    #[test]
    fn reparent_window_request_parses_all_fields() {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100002);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100003);
        write_i16(ClientByteOrder::LittleEndian, &mut body, -10);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 20);

        let req = reparent_window_request(&body).unwrap();
        assert_eq!(
            req,
            ReparentWindowRequest {
                window: ResourceId(0x100002),
                parent: ResourceId(0x100003),
                x: -10,
                y: 20,
            }
        );
    }

    #[test]
    fn reparent_window_request_rejects_short_body() {
        assert!(reparent_window_request(&[0; 11]).is_none());
    }

    #[test]
    fn reparent_notify_shape() {
        let mut buf = Vec::new();
        encode_reparent_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100),
            ResourceId(0x100002),
            ResourceId(0x100003),
            -5,
            7,
            true,
        );

        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 21);
        assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0x100003u32.to_le_bytes());
        assert_eq!(&buf[16..18], &(-5i16).to_le_bytes());
        assert_eq!(&buf[18..20], &7i16.to_le_bytes());
        assert_eq!(buf[20], 1);
        assert!(buf[21..].iter().all(|byte| *byte == 0));
    }
}

mod send_event_tests {
    use super::*;

    #[test]
    fn send_event_request_parses_payload() {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100002);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x00ff_0000);
        let event = [0xabu8; 32];
        body.extend_from_slice(&event);

        let req = send_event_request(1, &body).unwrap();
        assert!(req.propagate);
        assert_eq!(req.destination, ResourceId(0x100002));
        assert_eq!(req.event_mask, 0x00ff_0000);
        assert_eq!(req.event, &event);
    }

    #[test]
    fn send_event_request_rejects_short_body() {
        assert!(send_event_request(0, &[0; 39]).is_none());
    }

    #[test]
    fn client_message_encoder_shape() {
        let mut data = [0u8; 20];
        data[0] = 0xaa;
        data[19] = 0xbb;
        let mut buf = Vec::new();
        encode_client_message_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            ClientMessageEvent {
                sequence: SequenceNumber(0x1234),
                send_event: true,
                format: 32,
                window: ResourceId(0x100002),
                r#type: AtomId(0x44),
                data,
            },
        );

        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 33 | 0x80);
        assert_eq!(buf[1], 32);
        assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0x100002u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x44u32.to_le_bytes());
        assert_eq!(&buf[12..32], &data);
    }
}

mod pointer_event_tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn button_press_event_shape() {
        let mut buf = Vec::new();
        encode_button_press_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            PointerEvent {
                sequence: SequenceNumber(0x1234),
                detail: 1,
                time: 0xdead_beef,
                root: ResourceId(0x100),
                event: ResourceId(0x0010_0002),
                child: ResourceId(0),
                root_x: 100,
                root_y: 200,
                event_x: 10,
                event_y: 20,
                state: 0x0010,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 4); // ButtonPress
        assert_eq!(buf[1], 1); // detail
        assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0xdead_beefu32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0x0010_0002u32.to_le_bytes());
        assert_eq!(&buf[16..20], &0u32.to_le_bytes()); // child = 0
        assert_eq!(&buf[20..22], &100i16.to_le_bytes());
        assert_eq!(&buf[22..24], &200i16.to_le_bytes());
        assert_eq!(&buf[24..26], &10i16.to_le_bytes());
        assert_eq!(&buf[26..28], &20i16.to_le_bytes());
        assert_eq!(&buf[28..30], &0x0010u16.to_le_bytes());
        assert_eq!(buf[30], 1); // same_screen
        assert_eq!(buf[31], 0); // pad
    }

    #[test]
    fn button_release_event_shape() {
        let mut buf = Vec::new();
        encode_button_release_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            PointerEvent {
                sequence: SequenceNumber(0),
                detail: 2,
                time: 0,
                root: ResourceId(0x100),
                event: ResourceId(0x0010_0002),
                child: ResourceId(0),
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 5); // ButtonRelease
        assert_eq!(buf[1], 2); // detail
        assert_eq!(buf[30], 1); // same_screen
    }

    #[test]
    fn motion_notify_event_shape() {
        let mut buf = Vec::new();
        encode_motion_notify_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            PointerEvent {
                sequence: SequenceNumber(0),
                detail: 0,
                time: 0,
                root: ResourceId(0x100),
                event: ResourceId(0x0010_0002),
                child: ResourceId(0),
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 6); // MotionNotify
        assert_eq!(buf[1], 0); // detail = 0 for motion
        assert_eq!(buf[30], 1); // same_screen
    }

    #[test]
    fn enter_notify_event_shape() {
        let mut buf = Vec::new();
        encode_enter_notify_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            CrossingEvent {
                sequence: SequenceNumber(0x1234),
                time: 0xdead_beef,
                root: ResourceId(0x100),
                event: ResourceId(0x0010_0002),
                child: ResourceId(0x0010_0042),
                root_x: 100,
                root_y: 200,
                event_x: 10,
                event_y: 20,
                state: 0,
                detail: 0,
                mode: 0,
                focus: true,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 7); // EnterNotify
        assert_eq!(buf[1], 0); // detail = NotifyAncestor
        assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0xdead_beefu32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0x0010_0002u32.to_le_bytes());
        assert_eq!(&buf[16..20], &0x0010_0042u32.to_le_bytes()); // child
        assert_eq!(&buf[20..22], &100i16.to_le_bytes());
        assert_eq!(&buf[22..24], &200i16.to_le_bytes());
        assert_eq!(&buf[24..26], &10i16.to_le_bytes());
        assert_eq!(&buf[26..28], &20i16.to_le_bytes());
        assert_eq!(&buf[28..30], &0u16.to_le_bytes());
        assert_eq!(buf[30], 0); // mode = NotifyNormal
        assert_eq!(buf[31], 0x03); // ELFlagSameScreen 0x02 | ELFlagFocus 0x01
    }

    #[test]
    fn leave_notify_event_shape() {
        let mut buf = Vec::new();
        encode_leave_notify_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            CrossingEvent {
                sequence: SequenceNumber(0),
                time: 0,
                root: ResourceId(0x100),
                event: ResourceId(0x0010_0002),
                child: ResourceId(0),
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
                detail: 0,
                mode: 0,
                focus: true,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 8); // LeaveNotify
        assert_eq!(buf[1], 0); // detail = NotifyAncestor
        assert_eq!(buf[30], 0); // mode = NotifyNormal
        assert_eq!(buf[31], 0x03); // same_screen,focus
    }

    proptest! {
        #[test]
        fn pointer_encoder_round_trip(
            sequence in any::<u16>(),
            detail in any::<u8>(),
            time in any::<u32>(),
            root in any::<u32>(),
            event_window in any::<u32>(),
            child_xid in any::<u32>(),
            root_x in any::<i16>(),
            root_y in any::<i16>(),
            event_x in any::<i16>(),
            event_y in any::<i16>(),
            state in any::<u16>(),
            big_endian: bool,
            encoder_choice in 0u8..5,
        ) {
            let order = if big_endian {
                ClientByteOrder::BigEndian
            } else {
                ClientByteOrder::LittleEndian
            };
            let mut buf = Vec::new();
            let expected_code: u8;
            let expected_detail: u8;
            let expected_state_offset: usize = 28;
            match encoder_choice {
                0 => {
                    expected_code = 4;
                    expected_detail = detail;
                    encode_button_press_event(
                        &mut buf,
                        order,
                        PointerEvent {
                            sequence: SequenceNumber(sequence),
                            detail,
                            time,
                            root: ResourceId(root),
                            event: ResourceId(event_window),
                            root_x,
                            root_y,
                            event_x,
                            event_y,
                            child: ResourceId(child_xid),
                            state,
                        },
                    );
                }
                1 => {
                    expected_code = 5;
                    expected_detail = detail;
                    encode_button_release_event(
                        &mut buf,
                        order,
                        PointerEvent {
                            sequence: SequenceNumber(sequence),
                            detail,
                            time,
                            root: ResourceId(root),
                            event: ResourceId(event_window),
                            root_x,
                            root_y,
                            event_x,
                            event_y,
                            child: ResourceId(child_xid),
                            state,
                        },
                    );
                }
                2 => {
                    expected_code = 6;
                    expected_detail = detail;
                    encode_motion_notify_event(
                        &mut buf,
                        order,
                        PointerEvent {
                            sequence: SequenceNumber(sequence),
                            detail,
                            time,
                            root: ResourceId(root),
                            event: ResourceId(event_window),
                            root_x,
                            root_y,
                            event_x,
                            event_y,
                            child: ResourceId(child_xid),
                            state,
                        },
                    );
                }
                3 => {
                    expected_code = 7;
                    expected_detail = 0;
                    encode_enter_notify_event(
                        &mut buf,
                        order,
                        CrossingEvent {
                            sequence: SequenceNumber(sequence),
                            time,
                            root: ResourceId(root),
                            event: ResourceId(event_window),
                            child: ResourceId(child_xid),
                            root_x,
                            root_y,
                            event_x,
                            event_y,
                            state,
                            detail: 0,
                            mode: 0,
                            focus: true,
                        },
                    );
                }
                _ => {
                    expected_code = 8;
                    expected_detail = 0;
                    encode_leave_notify_event(
                        &mut buf,
                        order,
                        CrossingEvent {
                            sequence: SequenceNumber(sequence),
                            time,
                            root: ResourceId(root),
                            event: ResourceId(event_window),
                            child: ResourceId(child_xid),
                            root_x,
                            root_y,
                            event_x,
                            event_y,
                            state,
                            detail: 0,
                            mode: 0,
                            focus: true,
                        },
                    );
                }
            }

            prop_assert_eq!(buf.len(), 32);
            prop_assert_eq!(buf[0], expected_code);
            prop_assert_eq!(buf[1], expected_detail);

            let seq_bytes = if big_endian {
                sequence.to_be_bytes()
            } else {
                sequence.to_le_bytes()
            };
            prop_assert_eq!(&buf[2..4], &seq_bytes[..]);

            let time_bytes = if big_endian { time.to_be_bytes() } else { time.to_le_bytes() };
            prop_assert_eq!(&buf[4..8], &time_bytes[..]);

            let root_bytes = if big_endian { root.to_be_bytes() } else { root.to_le_bytes() };
            prop_assert_eq!(&buf[8..12], &root_bytes[..]);

            let event_bytes = if big_endian { event_window.to_be_bytes() } else { event_window.to_le_bytes() };
            prop_assert_eq!(&buf[12..16], &event_bytes[..]);

            let child_bytes = if big_endian {
                child_xid.to_be_bytes()
            } else {
                child_xid.to_le_bytes()
            };
            prop_assert_eq!(&buf[16..20], &child_bytes[..]);

            let rx = if big_endian { root_x.to_be_bytes() } else { root_x.to_le_bytes() };
            prop_assert_eq!(&buf[20..22], &rx[..]);
            let ry = if big_endian { root_y.to_be_bytes() } else { root_y.to_le_bytes() };
            prop_assert_eq!(&buf[22..24], &ry[..]);
            let ex = if big_endian { event_x.to_be_bytes() } else { event_x.to_le_bytes() };
            prop_assert_eq!(&buf[24..26], &ex[..]);
            let ey = if big_endian { event_y.to_be_bytes() } else { event_y.to_le_bytes() };
            prop_assert_eq!(&buf[26..28], &ey[..]);

            let state_bytes = if big_endian { state.to_be_bytes() } else { state.to_le_bytes() };
            prop_assert_eq!(&buf[expected_state_offset..expected_state_offset + 2], &state_bytes[..]);

            match expected_code {
                4..=6 => {
                    prop_assert_eq!(buf[30], 1); // same_screen
                    prop_assert_eq!(buf[31], 0); // pad
                }
                7 | 8 => {
                    prop_assert_eq!(buf[30], 0); // mode = NotifyNormal
                    prop_assert_eq!(buf[31], 0x03); // same_screen + focus
                }
                _ => unreachable!(),
            }
        }
    }
}

mod copy_area_tests {
    use super::*;

    #[test]
    fn all_fields_parse_correctly() {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x11111111);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x22222222);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x33333333);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 100);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 200);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 300);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 400);
        write_u16(ClientByteOrder::LittleEndian, &mut body, 500);
        write_u16(ClientByteOrder::LittleEndian, &mut body, 600);

        let req = copy_area_request(&body).unwrap();
        assert_eq!(req.src, ResourceId(0x11111111));
        assert_eq!(req.dst, ResourceId(0x22222222));
        assert_eq!(req.gc, ResourceId(0x33333333));
        assert_eq!(req.src_x, 100);
        assert_eq!(req.src_y, 200);
        assert_eq!(req.dst_x, 300);
        assert_eq!(req.dst_y, 400);
        assert_eq!(req.width, 500);
        assert_eq!(req.height, 600);
    }

    #[test]
    fn short_body_returns_none() {
        let body = [0u8; 23]; // 1 byte short
        assert!(copy_area_request(&body).is_none());
    }
}

mod put_image_tests {
    use super::*;

    #[test]
    fn z_pixmap_parses_all_scalar_fields_and_preserves_data_slice() {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x12345678);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 0x9abcdef0);
        write_u16(ClientByteOrder::LittleEndian, &mut body, 100);
        write_u16(ClientByteOrder::LittleEndian, &mut body, 200);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 50);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 75);
        body.push(5); // left_pad
        body.push(32); // depth
        body.extend_from_slice(&[0, 0]); // padding
        let data_slice = [0xAA, 0xBB, 0xCC, 0xDD];
        body.extend_from_slice(&data_slice);

        let req = put_image_request(2, &body).unwrap();
        assert_eq!(req.format, ImageFormat::ZPixmap);
        assert_eq!(req.drawable, ResourceId(0x12345678));
        assert_eq!(req.gc, ResourceId(0x9abcdef0));
        assert_eq!(req.width, 100);
        assert_eq!(req.height, 200);
        assert_eq!(req.dst_x, 50);
        assert_eq!(req.dst_y, 75);
        assert_eq!(req.left_pad, 5);
        assert_eq!(req.depth, 32);
        assert_eq!(req.data, &data_slice);
    }

    fn minimal_body() -> Vec<u8> {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, 1);
        write_u32(ClientByteOrder::LittleEndian, &mut body, 2);
        write_u16(ClientByteOrder::LittleEndian, &mut body, 3);
        write_u16(ClientByteOrder::LittleEndian, &mut body, 4);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 5);
        write_i16(ClientByteOrder::LittleEndian, &mut body, 6);
        body.push(7);
        body.push(8);
        body.extend_from_slice(&[0, 0]);
        body.push(0xAB);
        body
    }

    #[test]
    fn format_byte_0_maps_to_xy_bitmap() {
        let body = minimal_body();
        let req = put_image_request(0, &body).unwrap();
        assert_eq!(req.format, ImageFormat::XyBitmap);
    }

    #[test]
    fn format_byte_1_maps_to_xy_pixmap() {
        let body = minimal_body();
        let req = put_image_request(1, &body).unwrap();
        assert_eq!(req.format, ImageFormat::XyPixmap);
    }

    #[test]
    fn unknown_format_maps_to_unknown_value() {
        let body = minimal_body();
        let req = put_image_request(42, &body).unwrap();
        assert_eq!(req.format, ImageFormat::Unknown(42));
    }

    #[test]
    fn short_body_returns_none() {
        let body = [0u8; 19]; // 1 byte short of required 20 bytes
        assert!(put_image_request(2, &body).is_none());
    }
}

mod phase2_keyboard_tests {
    use super::*;

    #[test]
    fn parse_grab_key_request_basic() {
        let body = [
            0x12, 0x34, 0x00, 0x00, // grab_window 0x3412
            0x40, 0x00, // modifiers 0x0040
            24,   // keycode 24
            1,    // pointer_mode async
            1,    // keyboard_mode async
            0, 0, 0, // pad
        ];
        let parsed = parse_grab_key(&body, false).unwrap();
        assert_eq!(parsed.grab_window, 0x3412);
        assert_eq!(parsed.modifiers, 0x0040);
        assert_eq!(parsed.keycode, 24);
        assert_eq!(parsed.pointer_mode, 1);
        assert_eq!(parsed.keyboard_mode, 1);
        assert!(!parsed.owner_events);
    }

    #[test]
    fn parse_ungrab_key_request_basic() {
        let body = [0x12, 0x34, 0x00, 0x00, 0x40, 0x00, 0, 0];
        let parsed = parse_ungrab_key(&body, 24).unwrap();
        assert_eq!(parsed.grab_window, 0x3412);
        assert_eq!(parsed.keycode, 24);
        assert_eq!(parsed.modifiers, 0x0040);
    }

    #[test]
    fn mapping_notify_event_layout() {
        let mut buf = Vec::new();
        write_mapping_notify_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0),
            1,
            8,
            248,
        )
        .unwrap();
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 34);
        assert_eq!(buf[4], 1);
        assert_eq!(buf[5], 8);
        assert_eq!(buf[6], 248);
    }

    #[test]
    fn circulate_notify_event_layout() {
        let mut buf = Vec::new();
        write_circulate_notify_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0),
            ResourceId(0x100),
            ResourceId(0x200),
            0,
        )
        .unwrap();
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 26);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 0x100);
        assert_eq!(
            u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            0x200
        );
        assert_eq!(buf[16], 0);
    }

    #[test]
    fn circulate_request_event_layout() {
        let mut buf = Vec::new();
        write_circulate_request_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0),
            ResourceId(0x100),
            ResourceId(0x200),
            1,
        )
        .unwrap();
        assert_eq!(buf[0], 27);
        assert_eq!(buf[16], 1);
    }

    #[test]
    fn keyboard_mapping_reply_from_keysyms_layout() {
        let keysyms: &[u32] = &[0x71, 0x51, 0, 0, 0x77, 0x57, 0, 0];
        let mut buf = Vec::new();
        write_get_keyboard_mapping_reply_from_keysyms(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            4,
            keysyms,
        )
        .unwrap();
        assert_eq!(buf[0], 1);
        assert_eq!(buf[1], 4);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 8);
        assert_eq!(buf.len(), 32 + 8 * 4);
        assert_eq!(
            u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
            0x71
        );
    }

    #[test]
    fn modifier_mapping_reply_layout_kpm_2() {
        let kpm = 2u8;
        let kc: Vec<u8> = (0..(8 * kpm)).map(|i| i + 8).collect();
        let mut buf = Vec::new();
        write_get_modifier_mapping_reply_with_keycodes(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            kpm,
            &kc,
        )
        .unwrap();
        assert_eq!(buf[0], 1);
        assert_eq!(buf[1], kpm);
        let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(length, (8 * u32::from(kpm)) / 4);
        assert_eq!(&buf[32..32 + 8 * kpm as usize], &kc[..]);
    }

    #[test]
    fn modifier_mapping_reply_layout_kpm_4() {
        let kpm = 4u8;
        let kc: Vec<u8> = (0..(8 * kpm)).map(|i| i + 8).collect();
        let mut buf = Vec::new();
        write_get_modifier_mapping_reply_with_keycodes(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            kpm,
            &kc,
        )
        .unwrap();
        let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(length, (8 * u32::from(kpm)) / 4);
        assert_eq!(&buf[32..32 + 8 * kpm as usize], &kc[..]);
    }

    /// Walk the XI 1.x `ListInputDevices` reply byte-for-byte and
    /// assert it decodes to the 4-device model with consistent
    /// class/name framing. Guards against the empty-stub regression
    /// that crashed Chromium/Electron (it cross-checks this reply
    /// against XIQueryDevice).
    #[test]
    fn list_input_devices_reply_decodes_to_four_devices() {
        let le = ClientByteOrder::LittleEndian;
        // Simulated type atoms: MOUSE=69, KEYBOARD=70, TOUCHPAD=71
        // (the exact values are allocated at ServerState::with_geometry;
        // here we pass them explicitly to keep the test self-contained).
        const MOUSE: u32 = 69;
        const KEYBOARD: u32 = 70;
        let pointer_axes = [(-1, -1), (-1, -1), (-1, 0), (-1, 0)];
        let pointer_classes = [
            Xi1DeviceClass::Button { num_buttons: 7 },
            Xi1DeviceClass::Valuator {
                mode: 0,
                axes: &pointer_axes,
            },
        ];
        let keyboard_classes = [Xi1DeviceClass::Key {
            min_keycode: 8,
            max_keycode: 255,
            num_keys: 248,
        }];
        let devices = [
            Xi1DeviceDescriptor {
                id: 2,
                use_code: 0,
                attachment: 3,
                type_atom: AtomId(MOUSE),
                name: "Virtual core pointer",
                classes: &pointer_classes,
            },
            Xi1DeviceDescriptor {
                id: 3,
                use_code: 1,
                attachment: 2,
                type_atom: AtomId(KEYBOARD),
                name: "Virtual core keyboard",
                classes: &keyboard_classes,
            },
            Xi1DeviceDescriptor {
                id: 4,
                use_code: 4,
                attachment: 2,
                type_atom: AtomId(MOUSE),
                name: "Virtual core XTEST pointer",
                classes: &pointer_classes,
            },
            Xi1DeviceDescriptor {
                id: 5,
                use_code: 3,
                attachment: 3,
                type_atom: AtomId(KEYBOARD),
                name: "Virtual core XTEST keyboard",
                classes: &keyboard_classes,
            },
        ];
        let buf = encode_list_input_devices_reply(le, SequenceNumber(0x1234), &devices);

        // Header. Per XIproto.h, ndevices is at byte 8 — NOT the
        // standard reply "data" byte (byte 1). Byte 1 is RepType,
        // which clients ignore; a regression that put ndevices at
        // byte 1 made every client read 0 devices from byte 8.
        assert_eq!(buf[0], 1, "reply type");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234, "sequence");
        let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        assert_eq!(buf.len(), 32 + length * 4, "length covers data, 4-aligned");
        let ndevices = buf[8];
        assert_eq!(ndevices, 4, "ndevices at byte 8");

        // Device-info array (8B each), immediately after the 32B header.
        let mut off = 32;
        let mut dev = Vec::new();
        let expected_types = [MOUSE, KEYBOARD, MOUSE, KEYBOARD];
        for &expected_type in &expected_types {
            let type_atom =
                u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let id = buf[off + 4];
            let num_classes = buf[off + 5];
            let use_code = buf[off + 6];
            assert_eq!(
                type_atom, expected_type,
                "type ATOM for device {id} must match passed value"
            );
            assert_eq!(buf[off + 7], 0, "device-info pad");
            dev.push((id, num_classes, use_code));
            off += 8;
        }
        assert_eq!(
            dev,
            vec![(2, 2, 0), (3, 1, 1), (4, 2, 4), (5, 1, 3)],
            "(id, num_classes, use) per device matches XIQueryDevice model",
        );

        // Class-info blocks, per device, walked by the BYTE length
        // field (XI 1.x semantics, not 4-byte units).
        for &(id, num_classes, _) in &dev {
            for _ in 0..num_classes {
                let class = buf[off];
                let len = buf[off + 1] as usize;
                assert!(len >= 4 && off + len <= buf.len(), "class len in range");
                match class {
                    0 => {
                        // KeyClass: min, max, num_keys.
                        assert_eq!(len, 8);
                        assert_eq!(buf[off + 2], 8, "min keycode");
                        assert_eq!(buf[off + 3], 255, "max keycode");
                        assert_eq!(
                            u16::from_le_bytes([buf[off + 4], buf[off + 5]]),
                            248,
                            "num_keys",
                        );
                    }
                    1 => {
                        // ButtonClass: num_buttons.
                        assert_eq!(len, 4);
                        assert_eq!(
                            u16::from_le_bytes([buf[off + 2], buf[off + 3]]),
                            7,
                            "num_buttons",
                        );
                    }
                    2 => {
                        // ValuatorClass: num_axes, mode, 8 + 12n bytes.
                        let num_axes = buf[off + 2] as usize;
                        let mode = buf[off + 3];
                        assert_eq!(len, 8 + 12 * num_axes, "valuator byte length");
                        assert_eq!(num_axes, 4, "X, Y, vscroll, hscroll");
                        assert_eq!(mode, 0, "mode = Relative");
                    }
                    other => panic!("unexpected class id {other} for device {id}"),
                }
                off += len;
            }
        }

        // Name STR list: 1 length byte + bytes, in device order.
        let names = [
            "Virtual core pointer",
            "Virtual core keyboard",
            "Virtual core XTEST pointer",
            "Virtual core XTEST keyboard",
        ];
        for name in names {
            let n = buf[off] as usize;
            assert_eq!(n, name.len(), "name length byte");
            assert_eq!(&buf[off + 1..off + 1 + n], name.as_bytes(), "name bytes");
            off += 1 + n;
        }
        // After the names: one trailing NUL (Xorg parity) plus up
        // to 3 pad bytes, all zero. The per-name length bytes fully
        // describe the names, so a correct parser never reads here.
        assert!(buf.len() - off <= 4, "only trailing NUL + pad remains");
        assert!(
            buf[off..].iter().all(|&b| b == 0),
            "trailing bytes are zero"
        );
    }

    /// A seeded physical touchpad keeps its registry-assigned ID
    /// after the two XTEST devices; the XI1 reply must carry its
    /// name in the STR list (and matching length byte), proving
    /// names thread through from the registry rather than being
    /// hardcoded.
    #[test]
    fn list_input_devices_reply_reflects_seeded_pointer_facet_name() {
        let le = ClientByteOrder::LittleEndian;
        let touchpad = "SynPS/2 Synaptics TouchPad";
        let pointer_axes = [(-1, -1), (-1, -1), (-1, 0), (-1, 0)];
        let pointer_classes = [
            Xi1DeviceClass::Button { num_buttons: 7 },
            Xi1DeviceClass::Valuator {
                mode: 0,
                axes: &pointer_axes,
            },
        ];
        let keyboard_classes = [Xi1DeviceClass::Key {
            min_keycode: 8,
            max_keycode: 255,
            num_keys: 248,
        }];
        let devices = [
            Xi1DeviceDescriptor {
                id: 2,
                use_code: 0,
                attachment: 3,
                type_atom: AtomId(69),
                name: "Virtual core pointer",
                classes: &pointer_classes,
            },
            Xi1DeviceDescriptor {
                id: 3,
                use_code: 1,
                attachment: 2,
                type_atom: AtomId(70),
                name: "Virtual core keyboard",
                classes: &keyboard_classes,
            },
            Xi1DeviceDescriptor {
                id: 4,
                use_code: 4,
                attachment: 2,
                type_atom: AtomId(69),
                name: "Virtual core XTEST pointer",
                classes: &pointer_classes,
            },
            Xi1DeviceDescriptor {
                id: 5,
                use_code: 3,
                attachment: 3,
                type_atom: AtomId(70),
                name: "Virtual core XTEST keyboard",
                classes: &keyboard_classes,
            },
            Xi1DeviceDescriptor {
                id: 6,
                use_code: 4,
                attachment: 2,
                type_atom: AtomId(71),
                name: touchpad,
                classes: &pointer_classes,
            },
        ];
        let buf = encode_list_input_devices_reply(le, SequenceNumber(1), &devices);
        let ndevices = buf[8] as usize;
        assert_eq!(ndevices, 5);

        // Skip the device-info array.
        let mut off = 32 + 8 * ndevices;
        // Skip the class-info blocks (walk by byte length).
        for &num_classes in &[2u8, 1, 2, 1, 2] {
            for _ in 0..num_classes {
                let len = buf[off + 1] as usize;
                off += len;
            }
        }
        // Name STR list, device order: the 5th name is the touchpad.
        let expected = [
            "Virtual core pointer",
            "Virtual core keyboard",
            "Virtual core XTEST pointer",
            "Virtual core XTEST keyboard",
            touchpad,
        ];
        for name in expected {
            let n = buf[off] as usize;
            assert_eq!(n, name.len(), "name length byte for {name}");
            assert_eq!(&buf[off + 1..off + 1 + n], name.as_bytes());
            off += 1 + n;
        }
    }
}
