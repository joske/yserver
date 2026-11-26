use super::{
    access_notify_support::*,
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription},
    backend::recording::RecordedCall,
    core_loop::client_io::{self, WriteOutcome},
    resources::ROOT_WINDOW,
    server::PresentEventSelection,
    transport::Transport,
};
use std::os::fd::AsRawFd;
use yserver_protocol::x11::{
    ClientByteOrder, CreateGcRequest, CreatePixmapRequest, CreateWindowRequest, RequestHeader,
    ResourceId, SequenceNumber, present as x11present,
};

const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};
const COPY_SOURCE: ResourceId = ResourceId(0x0061_0001);
const COPY_DESTINATION: ResourceId = ResourceId(0x0061_0002);
const COPY_GC: ResourceId = ResourceId(0x0061_0003);

#[derive(Clone, Copy, Debug)]
struct CopyCase {
    opcode: u8,
    partial_source: bool,
    graphics_exposures: bool,
}

fn copy_fixture(graphics_exposures: bool) -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    for pixmap in [COPY_SOURCE, COPY_DESTINATION] {
        fixture.state.resources.create_pixmap(
            NOTIFY_SENDER,
            CreatePixmapRequest {
                pixmap,
                drawable: ROOT_WINDOW,
                width: 100,
                height: 100,
                depth: 24,
            },
        );
        assert!(fixture.state.resources.set_pixmap_host_xid(
            pixmap,
            crate::backend::PixmapHandle::from_raw_for_test(pixmap.0 | 0x0040_0000),
        ));
    }
    fixture.state.resources.create_gc(
        NOTIFY_SENDER,
        CreateGcRequest {
            gc: COPY_GC,
            drawable: COPY_DESTINATION,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: Some(graphics_exposures),
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );
    fixture
}

fn copy_request(fixture: &mut NotifyFixture, case: CopyCase) -> Vec<u8> {
    let (src_x, src_y, dst_x, dst_y, width, height) = if case.partial_source {
        // Crossing the source's top and left bounds produces two missing
        // destination rectangles, whose GraphicsExpose counts must be 1, 0.
        (-10_i16, -10_i16, 5_i16, 7_i16, 40_u16, 40_u16)
    } else {
        (10_i16, 10_i16, 5_i16, 7_i16, 30_u16, 40_u16)
    };
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&COPY_SOURCE.0.to_le_bytes());
    body.extend_from_slice(&COPY_DESTINATION.0.to_le_bytes());
    body.extend_from_slice(&COPY_GC.0.to_le_bytes());
    body.extend_from_slice(&src_x.to_le_bytes());
    body.extend_from_slice(&src_y.to_le_bytes());
    body.extend_from_slice(&dst_x.to_le_bytes());
    body.extend_from_slice(&dst_y.to_le_bytes());
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    if case.opcode == 63 {
        body.extend_from_slice(&1_u32.to_le_bytes());
    }
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(0x2a),
            RequestHeader {
                opcode: case.opcode,
                data: 0,
                length_units: u32::try_from((4 + body.len()) / 4).unwrap(),
            },
            &body,
        )
        .expect("CopyArea/CopyPlane through process_request");
    fixture.drain(NOTIFY_SENDER)
}

fn assert_copy_wire(bytes: &[u8], case: CopyCase) {
    let expected = if !case.graphics_exposures {
        Vec::new()
    } else if case.partial_source {
        let graphics_expose = |x, y, width, height, count| {
            let mut event = literal_event_header(13, 0, 0x2a, ClientByteOrder::LittleEndian);
            append_event_u32(
                &mut event,
                ClientByteOrder::LittleEndian,
                COPY_DESTINATION.0,
            );
            append_event_u16(&mut event, ClientByteOrder::LittleEndian, x);
            append_event_u16(&mut event, ClientByteOrder::LittleEndian, y);
            append_event_u16(&mut event, ClientByteOrder::LittleEndian, width);
            append_event_u16(&mut event, ClientByteOrder::LittleEndian, height);
            append_event_u16(&mut event, ClientByteOrder::LittleEndian, 0); // minor opcode
            append_event_u16(&mut event, ClientByteOrder::LittleEndian, count);
            event.push(case.opcode);
            finish_literal_event(event)
        };
        [
            graphics_expose(5, 7, 40, 10, 1),
            graphics_expose(5, 17, 10, 30, 0),
        ]
        .concat()
    } else {
        let mut event = literal_event_header(14, 0, 0x2a, ClientByteOrder::LittleEndian);
        append_event_u32(
            &mut event,
            ClientByteOrder::LittleEndian,
            COPY_DESTINATION.0,
        );
        append_event_u16(&mut event, ClientByteOrder::LittleEndian, 0); // minor opcode
        event.push(case.opcode);
        finish_literal_event(event)
    };
    assert_eq!(
        bytes, expected,
        "complete no-policy copy-result event frames: {case:?}"
    );
}

fn copy_run(
    case: CopyCase,
    deny_receive: bool,
) -> (Vec<u8>, Vec<RecordedCall>, Vec<RecordedExtensionCheck>) {
    let mut fixture = copy_fixture(case.graphics_exposures);
    let checks = deny_receive.then(|| {
        fixture.install_policy(|_, call| {
            if call.hook == ExtensionHook::Receive {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    let output = copy_request(&mut fixture, case);
    let backend = fixture.backend.calls();
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (output, backend, calls)
}

#[derive(Clone, Copy)]
enum MappingCase {
    Pointer,
    Modifier,
    Keyboard,
    XiKeyboard,
    XiModifier,
}

impl MappingCase {
    const ALL: [Self; 5] = [
        Self::Pointer,
        Self::Modifier,
        Self::Keyboard,
        Self::XiKeyboard,
        Self::XiModifier,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Pointer => "SetPointerMapping",
            Self::Modifier => "SetModifierMapping fallback",
            Self::Keyboard => "ChangeKeyboardMapping",
            Self::XiKeyboard => "XI ChangeDeviceKeyMapping",
            Self::XiModifier => "XI SetDeviceModifierMapping",
        }
    }

    const fn expected_mapping_request(self) -> u8 {
        match self {
            Self::Pointer => 2,
            Self::Modifier | Self::XiModifier => 0,
            Self::Keyboard | Self::XiKeyboard => 1,
        }
    }

    const fn expects_reply(self) -> bool {
        matches!(self, Self::Pointer | Self::Modifier | Self::XiModifier)
    }

    const fn expected_reply_status(self) -> u8 {
        match self {
            Self::XiModifier => 27,
            Self::Pointer | Self::Modifier | Self::Keyboard | Self::XiKeyboard => 0,
        }
    }

    const fn expected_mapping_range(self) -> (u8, u8) {
        match self {
            Self::Keyboard | Self::XiKeyboard => (8, 1),
            Self::Pointer | Self::Modifier | Self::XiModifier => (0, 0),
        }
    }
}

struct MappingRun {
    outputs: Vec<Vec<u8>>,
    pointer_mapping: Option<Vec<u8>>,
    modifier_mapping: Option<(u8, Vec<u8>)>,
    keyboard_mapping: Vec<(u8, Vec<u32>)>,
    backend: Vec<RecordedCall>,
    calls: Vec<RecordedExtensionCheck>,
}

fn mapping_run(case: MappingCase, deny_receive: bool) -> MappingRun {
    let mut fixture = NotifyFixture::new();
    let checks = deny_receive.then(|| {
        fixture.install_policy(|_, call| {
            if call.hook == ExtensionHook::Receive {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    let sequence = SequenceNumber(0x2b);
    let (opcode, data, body): (u8, u8, Vec<u8>) = match case {
        MappingCase::Pointer => {
            let mut map: Vec<u8> = (1..=10).collect();
            map.extend_from_slice(&[0, 0]);
            (116, 10, map)
        }
        MappingCase::Modifier | MappingCase::XiModifier => {
            let keys = [50_u8, 0, 37, 0, 0, 0, 0, 0];
            if matches!(case, MappingCase::Modifier) {
                (118, 1, keys.to_vec())
            } else {
                let mut body = vec![crate::xinput::DEVICEID_MASTER_KEYBOARD as u8, 1, 0, 0];
                body.extend_from_slice(&keys);
                (137, 27, body)
            }
        }
        MappingCase::Keyboard => {
            let mut body = vec![8, 1, 0, 0];
            body.extend_from_slice(&0x0061_u32.to_le_bytes());
            (100, 1, body)
        }
        MappingCase::XiKeyboard => {
            let mut body = vec![crate::xinput::DEVICEID_MASTER_KEYBOARD as u8, 8, 1, 1];
            body.extend_from_slice(&0x0061_u32.to_le_bytes());
            (137, 25, body)
        }
    };
    let length_units = u32::try_from((4 + body.len()).div_ceil(4)).unwrap();
    assert!(
        matches!(
            fixture.request(
                NOTIFY_SENDER,
                sequence,
                RequestHeader {
                    opcode,
                    data,
                    length_units,
                },
                &body,
            ),
            Ok(RequestOutcome::Handled)
        ),
        "{} request outcome",
        case.name()
    );

    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    MappingRun {
        outputs,
        pointer_mapping: fixture.state.pointer_mapping_override.clone(),
        modifier_mapping: fixture.state.modifier_mapping_override.clone(),
        keyboard_mapping: fixture
            .state
            .keymap_overrides
            .iter()
            .map(|(keycode, symbols)| (*keycode, symbols.clone()))
            .collect(),
        backend: fixture.backend.calls(),
        calls,
    }
}

fn assert_mapping_wire(run: &MappingRun, case: MappingCase) {
    let (first_keycode, count) = case.expected_mapping_range();
    let sender_mapping =
        literal_mapping_notify(0x2b, case.expected_mapping_request(), first_keycode, count);
    let receiver_mapping =
        literal_mapping_notify(0, case.expected_mapping_request(), first_keycode, count);
    let sender_reply = if case.expects_reply() {
        literal_empty_reply(0x2b, case.expected_reply_status())
    } else {
        Vec::new()
    };
    assert_eq!(
        run.outputs[0],
        [sender_mapping, sender_reply].concat(),
        "{} complete no-policy sender frames and reply",
        case.name()
    );
    assert_eq!(
        run.outputs[1],
        receiver_mapping,
        "{} complete no-policy receiver frame",
        case.name()
    );
    assert_eq!(
        run.outputs[2],
        receiver_mapping,
        "{} complete no-policy second receiver frame",
        case.name()
    );
}

fn literal_mapping_notify(sequence: u16, request: u8, first_keycode: u8, count: u8) -> Vec<u8> {
    let mut event = literal_event_header(34, 0, sequence, ClientByteOrder::LittleEndian);
    event.extend_from_slice(&[request, first_keycode, count]);
    finish_literal_event(event)
}

fn literal_empty_reply(sequence: u16, status: u8) -> Vec<u8> {
    let mut reply = vec![0; 32];
    reply[0] = 1;
    reply[1] = status;
    reply[2..4].copy_from_slice(&sequence.to_le_bytes());
    reply
}

fn literal_xi1_mapping_notify(sequence: u16) -> Vec<u8> {
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET;
    let mut event = literal_event_header(
        event_type,
        crate::xinput::DEVICEID_MASTER_KEYBOARD as u8,
        sequence,
        ClientByteOrder::LittleEndian,
    );
    event.extend_from_slice(&[1, 8, 1, 0]); // keyboard mapping, first keycode, count, pad
    append_event_u32(&mut event, ClientByteOrder::LittleEndian, 0); // fixture timestamp
    event.extend_from_slice(&[0; 20]);
    finish_literal_event(event)
}

fn literal_present_configure(sequence: u16) -> Vec<u8> {
    let mut event = [0_u8; 40];
    event[0] = 35; // GenericEvent
    event[1] = 145; // Present extension major opcode
    event[2..4].copy_from_slice(&sequence.to_le_bytes());
    event[4..8].copy_from_slice(&2_u32.to_le_bytes()); // two extra XGE words
    event[8..10].copy_from_slice(&u16::from(x11present::EVENT_CONFIGURE_NOTIFY).to_le_bytes());
    event[12..16].copy_from_slice(&0x0062_0042_u32.to_le_bytes());
    event[16..20].copy_from_slice(&NOTIFY_WINDOW.0.to_le_bytes());
    event[20..22].copy_from_slice(&610_i16.to_le_bytes());
    event[22..24].copy_from_slice(&250_i16.to_le_bytes());
    event[24..26].copy_from_slice(&50_u16.to_le_bytes());
    event[26..28].copy_from_slice(&50_u16.to_le_bytes());
    event[32..34].copy_from_slice(&50_u16.to_le_bytes());
    event[34..36].copy_from_slice(&50_u16.to_le_bytes());
    event.to_vec()
}

fn literal_core_configure(sequence: u16) -> Vec<u8> {
    let mut event = literal_event_header(22, 0, sequence, ClientByteOrder::LittleEndian);
    append_event_u32(&mut event, ClientByteOrder::LittleEndian, NOTIFY_WINDOW.0);
    append_event_u32(&mut event, ClientByteOrder::LittleEndian, NOTIFY_WINDOW.0);
    append_event_u32(&mut event, ClientByteOrder::LittleEndian, 0);
    event.extend_from_slice(&610_i16.to_le_bytes());
    event.extend_from_slice(&250_i16.to_le_bytes());
    append_event_u16(&mut event, ClientByteOrder::LittleEndian, 50);
    append_event_u16(&mut event, ClientByteOrder::LittleEndian, 50);
    append_event_u16(&mut event, ClientByteOrder::LittleEndian, 0);
    event.push(0); // override_redirect
    event.extend_from_slice(&[0; 5]);
    finish_literal_event(event)
}

fn receive_calls(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

// Mutations: change a no-policy GraphicsExpose/NoExpose/MappingNotify field
// or padding; add Receive before emit_copy_exposures' mutable client borrow;
// check MappingNotify at its fanout caller; classify failure SelectionNotify as a notification.
#[test]
fn receive_exempt_results_and_mapping_bypass_policy() {
    for opcode in [62, 63] {
        for partial_source in [false, true] {
            for graphics_exposures in [false, true] {
                let case = CopyCase {
                    opcode,
                    partial_source,
                    graphics_exposures,
                };
                let (baseline, baseline_backend, _) = copy_run(case, false);
                let (denied, denied_backend, calls) = copy_run(case, true);
                assert_copy_wire(&baseline, case);
                assert_eq!(
                    denied, baseline,
                    "direct copy result remains exempt: {case:?}"
                );
                assert_eq!(denied_backend, baseline_backend, "copy operation unchanged");
                assert!(
                    receive_calls(&calls).iter().all(|call| {
                        !call.events.as_ref().is_some_and(|events| {
                            events
                                .iter()
                                .any(|event| matches!(event.type_code, 13 | 14))
                        })
                    }),
                    "GraphicsExpose/NoExpose has no Receive call: {calls:?}"
                );
            }
        }
    }

    for case in MappingCase::ALL {
        let baseline = mapping_run(case, false);
        let denied = mapping_run(case, true);
        assert_mapping_wire(&baseline, case);
        assert_eq!(
            denied.outputs,
            baseline.outputs,
            "{} wire parity",
            case.name()
        );
        assert_eq!(
            denied.pointer_mapping,
            baseline.pointer_mapping,
            "{} pointer state",
            case.name()
        );
        assert_eq!(
            denied.modifier_mapping,
            baseline.modifier_mapping,
            "{} modifier state",
            case.name()
        );
        assert_eq!(
            denied.keyboard_mapping,
            baseline.keyboard_mapping,
            "{} keyboard state",
            case.name()
        );
        assert_eq!(
            denied.backend,
            baseline.backend,
            "{} backend state",
            case.name()
        );
        assert!(
            receive_calls(&denied.calls).iter().all(|call| {
                !call
                    .events
                    .as_ref()
                    .is_some_and(|events| events.iter().any(|event| event.type_code == 34))
            }),
            "MappingNotify is exempt from Receive: {:?}",
            denied.calls
        );
    }
}

fn buffered_property_output(allow_policy: bool) -> Vec<u8> {
    let mut fixture = NotifyFixture::new();
    select_property_change(&mut fixture);
    let property = fixture.state.atoms.intern("ACCESS_NOTIFY_BUFFERED", false);

    // Constrain the receiver's socket and queue a bounded filler so the
    // PropertyNotify is appended through ClientState::outbound.
    {
        let writer = fixture.state.clients[&NOTIFY_RECEIVER_ALLOWED.0]
            .writer
            .clone();
        let writer = writer.lock().unwrap();
        let Transport::Unix(stream) = &*writer else {
            panic!("NotifyFixture uses Unix socket clients");
        };
        stream
            .set_nonblocking(true)
            .expect("nonblocking test writer");
        let send_buffer = 4096_i32;
        // SAFETY: setsockopt reads the live integer for the socket's SO_SNDBUF.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::addr_of!(send_buffer).cast(),
                std::mem::size_of_val(&send_buffer) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "set small sender buffer");
        // FreeBSD AF_UNIX streams are bounded by the peer's receive buffer,
        // not by SO_SNDBUF, so shrink that too (as `client_io`'s tests do).
        let peer = &fixture.peers[&NOTIFY_RECEIVER_ALLOWED.0];
        // SAFETY: setsockopt reads the live integer for the peer's SO_RCVBUF.
        let result = unsafe {
            libc::setsockopt(
                peer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                std::ptr::addr_of!(send_buffer).cast(),
                std::mem::size_of_val(&send_buffer) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "set small receiver buffer");
    }
    {
        let client = fixture
            .state
            .clients
            .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
            .expect("allowed receiver");
        let filler = vec![0xa5; 32 * 1024];
        assert_eq!(
            client_io::write_or_buffer(client, &filler).unwrap(),
            WriteOutcome::WouldBlock,
            "bounded filler saturates the small socket"
        );
        assert!(!client.outbound.is_empty(), "filler is buffered");
    }

    if allow_policy {
        fixture.install_policy(|_, _| Decision::Allow);
    }
    assert!(matches!(
        run_property_change(&mut fixture, 0x33, property, b"buffered"),
        RequestOutcome::Handled
    ));
    let queued = fixture.state.clients[&NOTIFY_RECEIVER_ALLOWED.0]
        .outbound
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let expected = literal_property_notify(ClientByteOrder::LittleEndian, 0x5678, property);
    assert!(
        queued.ends_with(&expected),
        "buffered output ends with the complete literal PropertyNotify frame"
    );
    let output = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    let expected_output = [vec![0xa5; 32 * 1024], expected].concat();
    assert_eq!(
        output, expected_output,
        "complete buffered and drained output"
    );
    output
}

fn synthetic_selection_notify(allow_policy: bool) -> (Vec<u8>, Vec<RecordedExtensionCheck>) {
    let mut fixture = NotifyFixture::new();
    let requestor = ResourceId(0x0063_0001);
    fixture.state.resources.create_window(
        NOTIFY_RECEIVER_DENIED,
        CreateWindowRequest {
            depth: 24,
            window: requestor,
            parent: ROOT_WINDOW,
            width: 20,
            height: 20,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let client = fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .unwrap();
    client.byte_order = ClientByteOrder::BigEndian;
    client
        .last_sequence
        .store(0x4321, std::sync::atomic::Ordering::Relaxed);
    let selection = fixture
        .state
        .atoms
        .intern("ACCESS_SYNTHETIC_SELECTION", false);
    let target = fixture.state.atoms.intern("ACCESS_SYNTHETIC_TARGET", false);
    let property = fixture
        .state
        .atoms
        .intern("ACCESS_SYNTHETIC_PROPERTY", false);
    let checks = allow_policy.then(|| fixture.install_policy(|_, _| Decision::Allow));
    crate::rootless::selection::send_selection_notify(
        &mut fixture.state,
        0x1122_3344,
        requestor,
        [selection, target, property],
    )
    .expect("host SelectionNotify for the requestor window");

    let mut expected = [0_u8; 32];
    expected[0] = 31 | 0x80;
    expected[2..4].copy_from_slice(&0x4321_u16.to_be_bytes());
    for (offset, value) in [
        (4, 0x1122_3344),
        (8, requestor.0),
        (12, selection.0),
        (16, target.0),
        (20, property.0),
    ] {
        expected[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    let output = fixture.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        output, expected,
        "synthetic bit, BE fields, and recipient sequence"
    );
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (output, calls)
}

// Mutations: clear the synthetic SelectionNotify bit or change a buffered
// no-policy PropertyNotify byte or padding.
#[test]
fn receive_no_policy_aggregate_paths_match_expected_bytes() {
    let (synthetic_baseline, _) = synthetic_selection_notify(false);
    let (synthetic_allow, synthetic_calls) = synthetic_selection_notify(true);
    assert_eq!(
        synthetic_allow, synthetic_baseline,
        "recording-Allow synthetic SelectionNotify parity"
    );
    assert_eq!(receive_calls(&synthetic_calls).len(), 1);
    assert_eq!(
        receive_calls(&synthetic_calls)[0].events,
        Some(vec![EventDescription::core(31)])
    );
    let buffered_baseline = buffered_property_output(false);
    let buffered_allow = buffered_property_output(true);
    assert_eq!(
        buffered_allow, buffered_baseline,
        "recording-Allow buffered PropertyNotify parity"
    );
}

fn run_xi_mapping(
    deny_receive: bool,
) -> (Vec<Vec<u8>>, Vec<RecordedExtensionCheck>, Option<Vec<u32>>) {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET;
    let class = (u32::from(crate::xinput::DEVICEID_MASTER_KEYBOARD) << 8) | u32::from(event_type);
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[class]);
    let selection_outcome = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(1),
            RequestHeader {
                opcode: 137,
                data: 6,
                length_units: u32::try_from((4 + body.len()) / 4).unwrap(),
            },
            &body,
        )
        .expect("XI SelectExtensionEvent");
    assert!(matches!(selection_outcome, RequestOutcome::Handled));
    assert!(
        fixture.state.clients[&NOTIFY_SENDER.0]
            .xi1_event_classes
            .contains(&class)
    );
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        assert!(fixture.drain(client).is_empty(), "selection has no output");
    }
    let checks = deny_receive.then(|| {
        fixture.install_policy(|_, call| {
            if call.hook == ExtensionHook::Receive {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    let mut change = vec![crate::xinput::DEVICEID_MASTER_KEYBOARD as u8, 8, 1, 1];
    change.extend_from_slice(&0x0061_u32.to_le_bytes());
    let mapping_outcome = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(2),
            RequestHeader {
                opcode: 137,
                data: 25,
                length_units: 3,
            },
            &change,
        )
        .expect("XI ChangeDeviceKeyMapping");
    assert!(matches!(mapping_outcome, RequestOutcome::Handled));
    assert!(crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty());
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    let keymap = fixture.state.keymap_overrides.get(&8).cloned();
    (outputs, calls, keymap)
}

fn run_xkb_mapping(
    deny_receive: bool,
) -> (Vec<Vec<u8>>, Vec<RecordedExtensionCheck>, Vec<ClientId>) {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    assert!(crate::core_loop::xkb_select::use_extension(
        &mut fixture.state,
        NOTIFY_SENDER,
        &[1, 0, 0, 0]
    ));
    crate::core_loop::xkb_select::xkb_select_events(
        &mut fixture.state,
        NOTIFY_SENDER.0,
        0x0100,
        0x0002,
    );
    let mut reply = vec![0_u8; 32];
    reply[0] = 1;
    fixture.backend = RecordingBackend::new().with_kbd_by_name_result(
        reply,
        Some(crate::backend::XkbNewKeyboardInfo {
            min_keycode: 8,
            max_keycode: 255,
            old_min_keycode: 8,
            old_max_keycode: 255,
            changed: 1,
        }),
    );
    fixture.backend.xkb_info_result = Some((136, 85, 0));
    let checks = deny_receive.then(|| {
        fixture.install_policy(|_, call| {
            if call.hook == ExtensionHook::Receive {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    let receive_order: Vec<_> = fixture
        .state
        .clients
        .keys()
        .copied()
        .map(ClientId)
        .collect();
    let outcome = fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(3),
            RequestHeader {
                opcode: 136,
                data: 23,
                length_units: 1,
            },
            &[],
        )
        .expect("XkbGetKbdByName");
    assert!(matches!(outcome, RequestOutcome::Handled));
    assert!(
        client_io::failed_writers(&fixture.state.clients).is_empty(),
        "XKB notification fanout has no failed writers"
    );
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (outputs, calls, receive_order)
}

fn run_present_configure(
    policy: Option<bool>,
) -> (Vec<u8>, Vec<RecordedExtensionCheck>, (i16, i16)) {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    fixture.state.resources.create_window(
        NOTIFY_SENDER,
        CreateWindowRequest {
            depth: 24,
            window: NOTIFY_WINDOW,
            parent: ROOT_WINDOW,
            width: 50,
            height: 50,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let window = fixture.state.resources.window_mut(NOTIFY_WINDOW).unwrap();
        window.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0x0062_0001));
        window.map_state = crate::resources::MapState::Viewable;
    }
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_SENDER.0)
        .unwrap()
        .event_masks
        .insert(NOTIFY_WINDOW, 1 << 17);
    fixture.state.present_event_selections.insert(
        0x0062_0042,
        PresentEventSelection {
            owner: NOTIFY_SENDER,
            window: NOTIFY_WINDOW,
            event_mask: x11present::EVENT_MASK_CONFIGURE_NOTIFY,
        },
    );
    let checks = policy.map(|deny_receive| {
        fixture.install_policy(move |_, call| {
            if deny_receive && call.hook == ExtensionHook::Receive {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        })
    });
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&NOTIFY_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&3_u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&610_i32.to_le_bytes());
    body.extend_from_slice(&250_i32.to_le_bytes());
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(4),
            RequestHeader {
                opcode: 12,
                data: 0,
                length_units: 5,
            },
            &body,
        )
        .expect("ConfigureWindow with Present selection");
    let output = fixture.drain(NOTIFY_SENDER);
    let receives = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    let position = {
        let window = fixture.state.resources.window(NOTIFY_WINDOW).unwrap();
        (window.x, window.y)
    };
    (output, receives, position)
}

// Mutations: change any no-policy XI1/XKB/Present/XFIXES event byte or padding;
// suppress XI1/XKB/Present/XFIXES output when Receive denies the core event.
#[test]
fn receive_deny_all_keeps_later_extension_writers() {
    let (xi_baseline, _, baseline_keymap) = run_xi_mapping(false);
    let xi_sender_expected = [
        literal_mapping_notify(2, 1, 8, 1),
        literal_xi1_mapping_notify(2),
    ]
    .concat();
    let xi_receiver_expected = literal_mapping_notify(0, 1, 8, 1);
    assert_eq!(
        xi_baseline[0], xi_sender_expected,
        "complete no-policy XI1 and core event frames"
    );
    assert_eq!(
        xi_baseline[1], xi_receiver_expected,
        "complete no-policy core event frame for receiver"
    );
    assert_eq!(
        xi_baseline[2], xi_receiver_expected,
        "complete no-policy core event frame for second receiver"
    );
    let (xi_denied, xi_calls, denied_keymap) = run_xi_mapping(true);
    assert_eq!(
        xi_denied[0],
        literal_mapping_notify(2, 1, 8, 1),
        "denied originator XI copy leaves the literal core MappingNotify"
    );
    assert_eq!(
        xi_denied[1], xi_baseline[1],
        "first receiver core event is unchanged"
    );
    assert_eq!(
        xi_denied[2], xi_baseline[2],
        "second receiver core event is unchanged"
    );
    assert_eq!(denied_keymap, baseline_keymap);
    assert_eq!(denied_keymap.as_deref(), Some([0x61].as_slice()));
    let xi_receive_calls = receive_calls(&xi_calls);
    assert_eq!(xi_receive_calls.len(), 1);
    assert_eq!(xi_receive_calls[0].client, NOTIFY_SENDER);
    assert_eq!(
        xi_receive_calls[0].receive,
        Some(crate::access::EventSubject::Device(
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
        ))
    );
    assert_eq!(
        xi_receive_calls[0].events,
        Some(vec![EventDescription::extension(
            crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
            137,
            u16::from(crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET),
        )])
    );
    assert_eq!(
        xi_receive_calls[0].request,
        Some(crate::access::RequestOpcodes {
            major: 137,
            minor: 25,
        })
    );

    let (xkb_baseline, _, _) = run_xkb_mapping(false);
    let mut expected_new_keyboard = [0_u8; 32];
    expected_new_keyboard[0] = 85;
    expected_new_keyboard[2..4].copy_from_slice(&3_u16.to_le_bytes());
    expected_new_keyboard[8] = 1; // device ID
    expected_new_keyboard[9] = 1; // old device ID
    expected_new_keyboard[10] = 8;
    expected_new_keyboard[11] = 255;
    expected_new_keyboard[12] = 8;
    expected_new_keyboard[13] = 255;
    expected_new_keyboard[14] = 136; // XkbGetKbdByName major opcode
    expected_new_keyboard[15] = 23; // XkbGetKbdByName minor opcode
    expected_new_keyboard[16..18].copy_from_slice(&1_u16.to_le_bytes());
    let xkb_sender_events = [
        literal_mapping_notify(3, 1, 8, 248),
        literal_mapping_notify(3, 0, 0, 0),
        expected_new_keyboard.to_vec(),
    ]
    .concat();
    let xkb_sender_expected = [xkb_sender_events, literal_empty_reply(3, 0)].concat();
    let mut expected_new_keyboard_receiver = expected_new_keyboard;
    expected_new_keyboard_receiver[2..4].fill(0);
    let xkb_receiver_expected = [
        literal_mapping_notify(0, 1, 8, 248),
        literal_mapping_notify(0, 0, 0, 0),
        expected_new_keyboard_receiver.to_vec(),
    ]
    .concat();
    assert_eq!(
        xkb_baseline[0], xkb_sender_expected,
        "complete no-policy XKB event and reply frames"
    );
    assert_eq!(
        xkb_baseline[1], xkb_receiver_expected,
        "complete no-policy XKB legacy frames for receiver"
    );
    assert_eq!(
        xkb_baseline[2], xkb_receiver_expected,
        "complete no-policy XKB legacy frames for second receiver"
    );
    let (xkb_denied, xkb_calls, xkb_receive_order) = run_xkb_mapping(true);
    let xkb_sender_denied_expected = [
        literal_mapping_notify(3, 1, 8, 248),
        literal_mapping_notify(3, 0, 0, 0),
        literal_empty_reply(3, 0),
    ]
    .concat();
    assert_eq!(
        xkb_denied[0], xkb_sender_denied_expected,
        "NewKeyboardNotify is denied but core MappingNotify and reply remain in order"
    );
    let xkb_receiver_denied_expected = [
        literal_mapping_notify(0, 1, 8, 248),
        literal_mapping_notify(0, 0, 0, 0),
    ]
    .concat();
    assert_eq!(xkb_denied[1], xkb_receiver_denied_expected);
    assert_eq!(xkb_denied[2], xkb_receiver_denied_expected);
    assert_eq!(
        xkb_calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Receive)
            .count(),
        3,
        "whole-log Receive count"
    );
    assert!(
        xkb_calls
            .iter()
            .all(|call| call.hook != ExtensionHook::Send)
    );
    let xkb_receive_calls = receive_calls(&xkb_calls);
    for (call, client) in xkb_receive_calls.iter().zip(xkb_receive_order) {
        assert_eq!(call.client, client);
        assert_eq!(call.receive, Some(crate::access::EventSubject::Device(1)));
        assert_eq!(
            call.events,
            Some(vec![EventDescription::extension(85, 136, 0)])
        );
        assert_eq!(
            call.request,
            Some(crate::access::RequestOpcodes {
                major: 136,
                minor: 23,
            })
        );
    }

    let (present_baseline, _, baseline_position) = run_present_configure(None);
    let present_expected = [literal_present_configure(4), literal_core_configure(4)].concat();
    assert_eq!(
        present_baseline, present_expected,
        "complete no-policy Present and core ConfigureNotify frames"
    );
    let (present_allow, allow_calls, allow_position) = run_present_configure(Some(false));
    assert_eq!(
        receive_calls(&allow_calls).len(),
        1,
        "one core ConfigureNotify Receive hook"
    );
    assert_eq!(allow_position, baseline_position);
    let (present_denied, deny_calls, denied_position) = run_present_configure(Some(true));
    assert_eq!(
        present_denied,
        literal_present_configure(4),
        "complete Present frame survives Receive denial"
    );
    assert_eq!(
        receive_calls(&deny_calls).len(),
        1,
        "one Receive hook before the core event"
    );
    assert_eq!(
        denied_position, baseline_position,
        "configure operation still succeeds"
    );
    assert_eq!(
        present_allow, present_baseline,
        "Allow policy preserves fixed wire bytes"
    );

    let (xfixes_baseline, _, xfixes_baseline_bounds) =
        super::access_notify_selection::run_xfixes_owner_change(false);
    let (xfixes_denied, xfixes_calls, xfixes_denied_bounds) =
        super::access_notify_selection::run_xfixes_owner_change(true);
    super::access_notify_selection::assert_xfixes_event_matches_except_timestamps(
        &xfixes_denied[1],
        xfixes_denied_bounds,
        &xfixes_baseline[1],
        xfixes_baseline_bounds,
    );
    assert_eq!(
        xfixes_denied[1].first(),
        Some(&crate::nested::XFIXES_FIRST_EVENT),
        "XFIXES event arrives under deny-all Receive"
    );
    assert!(
        xfixes_denied[0].is_empty(),
        "denied core SelectionClear is omitted"
    );
    assert_eq!(
        receive_calls(&xfixes_calls).len(),
        1,
        "only SelectionClear has a hook"
    );
    assert_eq!(
        receive_calls(&xfixes_calls)[0].events,
        Some(vec![EventDescription::core(29)])
    );
}
