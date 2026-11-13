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
    if !case.graphics_exposures {
        assert!(bytes.is_empty(), "graphics_exposures=False sends no event");
        return;
    }
    assert_eq!(bytes.len() % 32, 0, "complete direct event frames");
    let frames: Vec<_> = bytes.chunks_exact(32).collect();
    if case.partial_source {
        assert_eq!(frames.len(), 2, "two missing source bands");
        for (index, expected_count) in [1_u16, 0].into_iter().enumerate() {
            let frame = frames[index];
            assert_eq!(frame[0], 13, "GraphicsExpose");
            assert_eq!(u16::from_le_bytes(frame[2..4].try_into().unwrap()), 0x2a);
            assert_eq!(
                u32::from_le_bytes(frame[4..8].try_into().unwrap()),
                COPY_DESTINATION.0,
                "destination drawable"
            );
            assert_eq!(
                u16::from_le_bytes(frame[18..20].try_into().unwrap()),
                expected_count,
                "decreasing GraphicsExpose count"
            );
            assert_eq!(frame[20], case.opcode, "originating Copy opcode");
        }
    } else {
        assert_eq!(frames.len(), 1);
        let frame = frames[0];
        assert_eq!(frame[0], 14, "NoExpose");
        assert_eq!(u16::from_le_bytes(frame[2..4].try_into().unwrap()), 0x2a);
        assert_eq!(
            u32::from_le_bytes(frame[4..8].try_into().unwrap()),
            COPY_DESTINATION.0,
            "destination drawable"
        );
        assert_eq!(frame[10], case.opcode, "originating Copy opcode");
    }
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
    for (client_index, bytes) in run.outputs.iter().enumerate() {
        assert_eq!(bytes.len() % 32, 0, "{} complete frames", case.name());
        let frames: Vec<_> = bytes.chunks_exact(32).collect();
        assert_eq!(
            frames.iter().filter(|frame| frame[0] == 34).count(),
            1,
            "{} MappingNotify for client {client_index}",
            case.name()
        );
        let mapping = frames.iter().find(|frame| frame[0] == 34).unwrap();
        assert_eq!(
            mapping[4],
            case.expected_mapping_request(),
            "{}",
            case.name()
        );
        assert_eq!(
            frames.iter().filter(|frame| frame[0] == 1).count(),
            usize::from(client_index == 0 && case.expects_reply()),
            "{} direct reply placement",
            case.name()
        );
    }
}

fn receive_calls(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

// Mutations: add Receive before emit_copy_exposures' mutable client borrow;
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

    // Task 7's dispatch fixture pins that the absent/denied-owner direct
    // SelectionNotify(None) answer receives no code-31 Receive check.
    super::access_notify_selection::run_denied_owner_refusal_fixture();
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
        "PropertyNotify is present at the end of buffered output"
    );
    let output = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(&output[..32 * 1024], vec![0xa5; 32 * 1024]);
    assert_eq!(&output[32 * 1024..], expected);
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

// Mutations: force synthetic-bit/sequence fields to constants; gate the old
// writers; drop a later extension event. Every case below compares no-policy
// bytes with recording-Allow output and keeps its row's fixed wire assertions.
#[test]
fn receive_no_policy_core_bytes_match_baseline() {
    let fixtures: &[(&str, fn())] = &[
        (
            "N01–N30 window notifications and redirects",
            super::access_notify_windows::run_no_policy_parity_fixtures,
        ),
        (
            "N31–N36 destruction and COW notifications",
            super::access_notify_lifecycle::run_no_policy_parity_fixtures,
        ),
        (
            "N37–N38/N47 focus, colormap, and property",
            super::access_notify_objects::run_no_policy_parity_fixtures,
        ),
        (
            "N39 property fanout",
            super::access_notify_egress::run_no_policy_parity_fixtures,
        ),
        (
            "N40–N46 selection and rootless notifications",
            super::access_notify_selection::run_no_policy_parity_fixtures,
        ),
        (
            "Present before core ConfigureNotify",
            super::present_pixmap::run_no_policy_parity_fixtures,
        ),
    ];
    assert_eq!(fixtures.len(), 6, "enumerated no-policy fixture groups");
    let (synthetic_baseline, _) = synthetic_selection_notify(false);
    let (synthetic_allow, synthetic_calls) = synthetic_selection_notify(true);
    assert_eq!(synthetic_allow, synthetic_baseline);
    assert_eq!(receive_calls(&synthetic_calls).len(), 1);
    assert_eq!(
        receive_calls(&synthetic_calls)[0].events,
        Some(vec![EventDescription::core(31)])
    );
    for (_row, run) in fixtures {
        run();
    }
    receive_exempt_results_and_mapping_bypass_policy();
    receive_deny_all_keeps_later_extension_writers();
    buffered_property_output(false);
    buffered_property_output(true);
}

fn run_xi_mapping(deny_receive: bool) -> (Vec<Vec<u8>>, Vec<RecordedExtensionCheck>) {
    let mut fixture = NotifyFixture::new();
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET;
    let class = (u32::from(crate::xinput::DEVICEID_MASTER_KEYBOARD) << 8) | u32::from(event_type);
    let body = xi1_select_extension_event_body(ROOT_WINDOW.0, &[class]);
    fixture
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
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
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
    fixture
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
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (outputs, calls)
}

fn run_xkb_mapping(deny_receive: bool) -> (Vec<Vec<u8>>, Vec<RecordedExtensionCheck>) {
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
    fixture
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
    let outputs = [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| fixture.drain(client))
    .collect();
    let calls = checks.map_or_else(Vec::new, |calls| calls.lock().unwrap().clone());
    (outputs, calls)
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

// Mutation: suppress XI1/XKB/Present/XFIXES output in the shared old writers
// when Receive denies the neighboring core notification.
#[test]
fn receive_deny_all_keeps_later_extension_writers() {
    let (xi_baseline, _) = run_xi_mapping(false);
    let (xi_denied, xi_calls) = run_xi_mapping(true);
    assert_eq!(xi_denied, xi_baseline, "XI device mapping event survives");
    let xi_sender_frames: Vec<_> = xi_denied[0]
        .chunks_exact(32)
        .map(|frame| frame[0])
        .collect();
    assert_eq!(
        xi_sender_frames[0], 34,
        "core MappingNotify precedes XI1 event"
    );
    assert_eq!(
        xi_sender_frames[1],
        crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MAPPING_NOTIFY_OFFSET,
        "XI DeviceMappingNotify follows core MappingNotify"
    );
    assert!(
        receive_calls(&xi_calls).is_empty(),
        "XI/core mapping exemptions add no hooks"
    );

    let (xkb_baseline, _) = run_xkb_mapping(false);
    let (xkb_denied, xkb_calls) = run_xkb_mapping(true);
    assert_eq!(
        xkb_denied, xkb_baseline,
        "XKB and direct reply bytes survive"
    );
    assert_eq!(
        xkb_denied[0][0], 34,
        "XKB path starts with core MappingNotify: {:02x?}",
        xkb_denied[0]
    );
    assert_eq!(
        xkb_denied[0][32], 34,
        "modifier MappingNotify remains second"
    );
    assert_eq!(xkb_denied[0][64], 85, "XkbNewKeyboardNotify remains third");
    assert_eq!(
        xkb_denied[0][96], 1,
        "request reply remains after notifications"
    );
    assert!(
        receive_calls(&xkb_calls).is_empty(),
        "MappingNotify remains exempt"
    );

    let (present_baseline, _, baseline_position) = run_present_configure(None);
    let (present_allow, allow_calls, allow_position) = run_present_configure(Some(false));
    assert_eq!(
        present_baseline.len(),
        72,
        "Present then core ConfigureNotify"
    );
    assert_eq!(present_baseline[0], 35, "Present GenericEvent first");
    assert_eq!(
        present_baseline[40] & 0x7f,
        22,
        "core ConfigureNotify follows Present"
    );
    assert_eq!(
        receive_calls(&allow_calls).len(),
        1,
        "one core ConfigureNotify Receive hook"
    );
    assert_eq!(allow_position, baseline_position);
    let (present_denied, deny_calls, denied_position) = run_present_configure(Some(true));
    assert_eq!(
        present_denied.len(),
        40,
        "Present event still reaches its selector"
    );
    assert_eq!(present_denied[0], 35, "Present GenericEvent retained");
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

    let (xfixes_baseline, _) = super::access_notify_selection::run_xfixes_owner_change(false);
    let (xfixes_denied, xfixes_calls) =
        super::access_notify_selection::run_xfixes_owner_change(true);
    assert_eq!(
        xfixes_denied[1].first(),
        xfixes_baseline[1].first(),
        "XFIXES SelectionNotify event byte survives"
    );
    assert_eq!(xfixes_denied[1].len(), xfixes_baseline[1].len());
    let mut denied_xfixes = xfixes_denied[1].clone();
    let mut baseline_xfixes = xfixes_baseline[1].clone();
    // CurrentTime is derived from each fixture's independent monotonic start.
    denied_xfixes[16..20].copy_from_slice(&[0; 4]);
    baseline_xfixes[16..20].copy_from_slice(&[0; 4]);
    assert_eq!(
        denied_xfixes, baseline_xfixes,
        "XFIXES SelectionNotify payload survives Receive denial"
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
