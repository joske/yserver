use super::*;

fn randr_panning_body(crtc: u32, width: u16, height: u16) -> Vec<u8> {
    let mut body = vec![0u8; 32];
    body[0..4].copy_from_slice(&crtc.to_le_bytes());
    body[12..14].copy_from_slice(&width.to_le_bytes());
    body[14..16].copy_from_slice(&height.to_le_bytes());
    body
}

fn randr_provider(provider_id: u32, capabilities: u32) -> crate::randr::RandrProvider {
    crate::randr::RandrProvider {
        provider_id,
        name: format!("card{provider_id}"),
        capabilities,
        is_gpu: true,
        crtcs: Vec::new(),
        outputs: Vec::new(),
        associations: Vec::new(),
    }
}

fn randr_provider_request_body(minor: u8, provider: u32) -> Vec<u8> {
    use yserver_protocol::x11::randr as x11randr;

    let mut body = Vec::new();
    body.extend_from_slice(&provider.to_le_bytes());
    match minor {
        x11randr::RR_GET_PROVIDER_INFO => {
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        x11randr::RR_SET_PROVIDER_OFFLOAD_SINK | x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE => {
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        x11randr::RR_LIST_PROVIDER_PROPERTIES => {}
        x11randr::RR_QUERY_PROVIDER_PROPERTY | x11randr::RR_DELETE_PROVIDER_PROPERTY => {
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        x11randr::RR_CONFIGURE_PROVIDER_PROPERTY => {
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&[0u8; 4]);
        }
        x11randr::RR_CHANGE_PROVIDER_PROPERTY => {
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&[8, 0, 0, 0]);
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        x11randr::RR_GET_PROVIDER_PROPERTY => {
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&[0u8; 4]);
        }
        _ => panic!("not a provider request minor: {minor}"),
    }
    body
}

fn randr_output_source_body(provider: u32, source_provider: u32, timestamp: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&provider.to_le_bytes());
    body.extend_from_slice(&source_provider.to_le_bytes());
    body.extend_from_slice(&timestamp.to_le_bytes());
    body
}

#[test]
fn randr_set_panning_accepts_disabled_and_refuses_active_viewport() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let crtc = state.randr.outputs[0].crtc_id;
    let timestamp = state.randr.timestamp;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 128,
        data: x11randr::RR_SET_PANNING,
        length_units: 9,
    };

    let mut disabled = randr_panning_body(crtc, 0, 0);
    disabled[8..10].copy_from_slice(&15u16.to_le_bytes());
    disabled[24..26].copy_from_slice(&(-3i16).to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &disabled,
    )
    .expect("disabled panning");

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        header,
        &randr_panning_body(crtc, 1920, 1080),
    )
    .expect("active panning refusal");

    let replies = read_all_available(&mut peer);
    assert_eq!(replies.len(), 64);
    assert_eq!(replies[0], 1);
    assert_eq!(replies[1], x11randr::SET_CONFIG_SUCCESS);
    assert_eq!(
        u32::from_le_bytes(replies[8..12].try_into().unwrap()),
        timestamp
    );
    assert_eq!(replies[32], 1);
    assert_eq!(replies[33], x11randr::SET_CONFIG_FAILED);
    assert_ne!(
        state.randr_unsupported_warned_mask & (1 << x11randr::RR_SET_PANNING),
        0,
    );
}

#[test]
fn randr_provider_queries_report_sorted_topology_in_both_byte_orders() {
    use yserver_protocol::x11::randr as x11randr;

    for byte_order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        state.randr.set_providers(vec![
            randr_provider(20, 0),
            crate::randr::RandrProvider {
                name: "card0".to_string(),
                capabilities: x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT,
                crtcs: vec![2],
                outputs: vec![1],
                associations: vec![crate::randr::RandrProviderAssociation {
                    provider_id: 20,
                    capability: x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT,
                }],
                ..randr_provider(10, 0)
            },
        ]);
        let mut peer = install_client(&mut state, 1);
        state.clients.get_mut(&1).expect("test client").byte_order = byte_order;
        let mut backend = RecordingBackend::new();

        let mut providers_body = match byte_order {
            ClientByteOrder::LittleEndian => ROOT_WINDOW.0.to_le_bytes().to_vec(),
            ClientByteOrder::BigEndian => ROOT_WINDOW.0.to_be_bytes().to_vec(),
        };
        yserver_protocol::x11::request_swap::swap_request_body(
            128,
            x11randr::RR_GET_PROVIDERS,
            byte_order,
            &mut providers_body,
        );
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: 128,
                data: x11randr::RR_GET_PROVIDERS,
                length_units: 2,
            },
            &providers_body,
        )
        .expect("get providers");
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 40);
        match byte_order {
            ClientByteOrder::LittleEndian => {
                assert_eq!(&bytes[4..8], &2u32.to_le_bytes());
                assert_eq!(&bytes[12..14], &2u16.to_le_bytes());
                assert_eq!(&bytes[32..36], &10u32.to_le_bytes());
                assert_eq!(&bytes[36..40], &20u32.to_le_bytes());
            }
            ClientByteOrder::BigEndian => {
                assert_eq!(&bytes[4..8], &2u32.to_be_bytes());
                assert_eq!(&bytes[12..14], &2u16.to_be_bytes());
                assert_eq!(&bytes[32..36], &10u32.to_be_bytes());
                assert_eq!(&bytes[36..40], &20u32.to_be_bytes());
            }
        }

        let mut info_body = Vec::new();
        match byte_order {
            ClientByteOrder::LittleEndian => {
                info_body.extend_from_slice(&10u32.to_le_bytes());
                info_body.extend_from_slice(&0u32.to_le_bytes());
            }
            ClientByteOrder::BigEndian => {
                info_body.extend_from_slice(&10u32.to_be_bytes());
                info_body.extend_from_slice(&0u32.to_be_bytes());
            }
        }
        yserver_protocol::x11::request_swap::swap_request_body(
            128,
            x11randr::RR_GET_PROVIDER_INFO,
            byte_order,
            &mut info_body,
        );
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(2),
            RequestHeader {
                opcode: 128,
                data: x11randr::RR_GET_PROVIDER_INFO,
                length_units: 3,
            },
            &info_body,
        )
        .expect("get provider info");
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 56);
        assert_eq!(bytes[1], x11randr::SET_CONFIG_SUCCESS);
        match byte_order {
            ClientByteOrder::LittleEndian => {
                assert_eq!(&bytes[4..8], &6u32.to_le_bytes());
                assert_eq!(
                    &bytes[12..16],
                    &x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT.to_le_bytes()
                );
                assert_eq!(&bytes[16..18], &1u16.to_le_bytes());
                assert_eq!(&bytes[18..20], &1u16.to_le_bytes());
                assert_eq!(&bytes[20..22], &1u16.to_le_bytes());
                assert_eq!(&bytes[32..36], &2u32.to_le_bytes());
                assert_eq!(&bytes[36..40], &1u32.to_le_bytes());
                assert_eq!(&bytes[40..44], &20u32.to_le_bytes());
                assert_eq!(
                    &bytes[44..48],
                    &x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT.to_le_bytes()
                );
            }
            ClientByteOrder::BigEndian => {
                assert_eq!(&bytes[4..8], &6u32.to_be_bytes());
                assert_eq!(
                    &bytes[12..16],
                    &x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT.to_be_bytes()
                );
                assert_eq!(&bytes[16..18], &1u16.to_be_bytes());
                assert_eq!(&bytes[18..20], &1u16.to_be_bytes());
                assert_eq!(&bytes[20..22], &1u16.to_be_bytes());
                assert_eq!(&bytes[32..36], &2u32.to_be_bytes());
                assert_eq!(&bytes[36..40], &1u32.to_be_bytes());
                assert_eq!(&bytes[40..44], &20u32.to_be_bytes());
                assert_eq!(
                    &bytes[44..48],
                    &x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT.to_be_bytes()
                );
            }
        }
        assert_eq!(&bytes[48..53], b"card0");
        assert!(bytes[53..].iter().all(|byte| *byte == 0));
    }
}

#[test]
fn randr_provider_requests_validate_exact_or_minimum_wire_size_first() {
    use yserver_protocol::x11::randr as x11randr;

    let mut valid_bodies = vec![(
        x11randr::RR_GET_PROVIDERS,
        ROOT_WINDOW.0.to_le_bytes().to_vec(),
    )];
    valid_bodies
        .extend((33..=41).map(|minor| (minor, randr_provider_request_body(minor, 0x00ab_cdef))));
    for (minor, valid) in valid_bodies {
        let mut malformed_bodies = vec![valid[..valid.len() - 4].to_vec()];
        if minor != x11randr::RR_CONFIGURE_PROVIDER_PROPERTY {
            let mut extra = valid.clone();
            extra.extend_from_slice(&[0; 4]);
            malformed_bodies.push(extra);
        }
        for malformed in malformed_bodies {
            let mut state = ServerState::new();
            let mut peer = install_client(&mut state, 1);
            let mut backend = RecordingBackend::new();
            handle_randr_request(
                &mut state,
                &mut backend,
                ClientId(1),
                SequenceNumber(u16::from(minor)),
                RequestHeader {
                    opcode: 128,
                    data: minor,
                    length_units: 1 + malformed.len().div_ceil(4) as u32,
                },
                &malformed,
            )
            .expect("process malformed provider request");

            let bytes = read_all_available(&mut peer);
            assert_eq!(bytes.len(), 32, "minor {minor} must return an error");
            assert_eq!(bytes[1], x11::error::BAD_LENGTH, "minor {minor} code");
            assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0);
        }
    }
}

#[test]
fn randr_unknown_provider_requests_use_valid_shapes_then_bad_provider() {
    const PROVIDER: u32 = 0x00ab_cdef;
    for minor in 33..=41 {
        let body = randr_provider_request_body(minor, PROVIDER);
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(u16::from(minor)),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units: 1 + (body.len() / 4) as u32,
            },
            &body,
        )
        .expect("process provider request");

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32, "minor {minor} must return an error");
        assert_eq!(bytes[1], RANDR_BAD_PROVIDER, "minor {minor} error");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            PROVIDER,
            "minor {minor} bad provider",
        );
        assert_eq!(&bytes[8..10], &u16::from(minor).to_le_bytes());
        assert_eq!(bytes[10], 128);
    }
}

#[test]
fn randr_provider_relationships_validate_roles_in_xorg_order() {
    use yserver_protocol::x11::randr as x11randr;

    let cases: &[(u8, u32, u32, u8, u32)] = &[
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            99,
            0,
            RANDR_BAD_PROVIDER,
            99,
        ),
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            5,
            99,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            1,
            99,
            RANDR_BAD_PROVIDER,
            99,
        ),
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            1,
            5,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            6,
            99,
            RANDR_BAD_PROVIDER,
            99,
        ),
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            6,
            2,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            6,
            0,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            99,
            0,
            RANDR_BAD_PROVIDER,
            99,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            5,
            99,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            7,
            99,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            7,
            0,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            3,
            99,
            RANDR_BAD_PROVIDER,
            99,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            3,
            5,
            x11::error::BAD_VALUE,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            3,
            0,
            x11::error::BAD_IMPLEMENTATION,
            0,
        ),
        (
            x11randr::RR_SET_PROVIDER_OFFLOAD_SINK,
            3,
            4,
            x11::error::BAD_IMPLEMENTATION,
            0,
        ),
    ];
    for &(minor, provider, peer_provider, error_code, error_value) in cases {
        let mut state = ServerState::new();
        state.randr.set_providers(vec![
            randr_provider(1, x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT),
            randr_provider(2, x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT),
            randr_provider(3, x11randr::PROVIDER_CAPABILITY_SOURCE_OFFLOAD),
            randr_provider(4, x11randr::PROVIDER_CAPABILITY_SINK_OFFLOAD),
            randr_provider(5, 0),
            crate::randr::RandrProvider {
                is_gpu: false,
                ..randr_provider(6, x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT)
            },
            crate::randr::RandrProvider {
                is_gpu: false,
                ..randr_provider(7, x11randr::PROVIDER_CAPABILITY_SOURCE_OFFLOAD)
            },
        ]);
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let mut body = Vec::new();
        body.extend_from_slice(&provider.to_le_bytes());
        body.extend_from_slice(&peer_provider.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(u16::from(minor)),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units: 4,
            },
            &body,
        )
        .expect("process provider relationship");
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], error_code, "minor={minor} provider={provider}");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            error_value,
            "minor={minor} provider={provider}",
        );
    }
}

#[test]
fn randr_set_provider_output_source_reaches_backend_for_attach_and_detach() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    state.randr.set_providers(vec![
        randr_provider(10, x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT),
        randr_provider(11, x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT),
    ]);
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    for (sequence, source_provider) in [(1, 10), (2, 0)] {
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 128,
                data: x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
                length_units: 4,
            },
            &randr_output_source_body(11, source_provider, 77),
        )
        .expect("set provider output source");
        assert!(
            read_all_available(&mut peer).is_empty(),
            "successful void requests do not emit a reply"
        );
    }

    assert_eq!(
        backend.calls(),
        vec![
            RecordedCall::SetProviderOutputSource {
                provider: 11,
                source_provider: Some(10),
            },
            RecordedCall::SetProviderOutputSource {
                provider: 11,
                source_provider: None,
            },
        ]
    );
}

#[test]
fn randr_set_provider_output_source_notifies_in_both_byte_orders() {
    use yserver_protocol::x11::randr as x11randr;

    for byte_order in [ClientByteOrder::LittleEndian, ClientByteOrder::BigEndian] {
        let mut state = ServerState::new();
        state.randr.timestamp = 0x0102_0304;
        state.randr.config_timestamp = 0x0506_0708;
        state.randr.set_providers(vec![
            randr_provider(10, x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT),
            randr_provider(11, x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT),
        ]);
        let mut peer = install_client(&mut state, 1);
        let client = state.clients.get_mut(&1).expect("test client");
        client.byte_order = byte_order;
        client
            .last_sequence
            .store(14, std::sync::atomic::Ordering::Relaxed);
        state
            .randr_select_masks
            .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_PROVIDER_CHANGE);
        let mut backend = RecordingBackend::new();

        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(14),
            RequestHeader {
                opcode: 128,
                data: x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
                length_units: 4,
            },
            &randr_output_source_body(11, 10, 99),
        )
        .expect("set provider output source");

        let event = read_all_available(&mut peer);
        let (sequence, timestamp, request_window, provider) = match byte_order {
            ClientByteOrder::LittleEndian => (
                14u16.to_le_bytes(),
                0x0102_0304u32.to_le_bytes(),
                ROOT_WINDOW.0.to_le_bytes(),
                11u32.to_le_bytes(),
            ),
            ClientByteOrder::BigEndian => (
                14u16.to_be_bytes(),
                0x0102_0304u32.to_be_bytes(),
                ROOT_WINDOW.0.to_be_bytes(),
                11u32.to_be_bytes(),
            ),
        };
        assert_eq!(event.len(), 32);
        assert_eq!(event[0], 90, "RANDR Notify event");
        assert_eq!(event[1], x11randr::NOTIFY_PROVIDER_CHANGE);
        assert_eq!(&event[2..4], &sequence);
        assert_eq!(&event[4..8], &timestamp);
        assert_eq!(&event[8..12], &request_window);
        assert_eq!(&event[12..16], &provider);
        assert_eq!(state.randr.timestamp, 0x0102_0304);
        assert_eq!(state.randr.config_timestamp, 0x0506_0708);
    }
}

#[test]
fn randr_idempotent_provider_output_source_skips_notify() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    state.randr.timestamp = 40;
    state.randr.config_timestamp = 50;
    state.randr.set_providers(vec![
        randr_provider(10, x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT),
        randr_provider(11, x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT),
    ]);
    let mut peer = install_client(&mut state, 1);
    state
        .randr_select_masks
        .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_PROVIDER_CHANGE);
    let mut backend = RecordingBackend::new();
    backend.provider_output_source_changed = false;

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
            length_units: 4,
        },
        &randr_output_source_body(11, 10, 99),
    )
    .expect("idempotent provider output source");

    assert!(read_all_available(&mut peer).is_empty());
    assert_eq!(state.randr.timestamp, 40);
    assert_eq!(state.randr.config_timestamp, 50);
    assert_eq!(
        backend.calls(),
        vec![RecordedCall::SetProviderOutputSource {
            provider: 11,
            source_provider: Some(10),
        }]
    );
}

#[test]
fn randr_provider_output_source_maps_backend_errors_by_kind() {
    use yserver_protocol::x11::randr as x11randr;

    for (kind, error_code, error_value) in [
        (io::ErrorKind::InvalidInput, x11::error::BAD_MATCH, 11),
        (io::ErrorKind::Other, x11::error::BAD_IMPLEMENTATION, 0),
    ] {
        let mut state = ServerState::new();
        state.randr.set_providers(vec![
            randr_provider(10, x11randr::PROVIDER_CAPABILITY_SOURCE_OUTPUT),
            randr_provider(11, x11randr::PROVIDER_CAPABILITY_SINK_OUTPUT),
        ]);
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        backend.provider_output_source_error = Some(kind);

        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: 128,
                data: x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE,
                length_units: 4,
            },
            &randr_output_source_body(11, 10, 99),
        )
        .expect("backend provider output source error");

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[1], error_code, "backend error kind {kind:?}");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            error_value,
            "backend error kind {kind:?}"
        );
    }
}

#[test]
fn randr_known_provider_property_requests_are_explicitly_unsupported() {
    use yserver_protocol::x11::randr as x11randr;

    for minor in x11randr::RR_LIST_PROVIDER_PROPERTIES..=x11randr::RR_GET_PROVIDER_PROPERTY {
        let mut body = randr_provider_request_body(minor, 10);
        if minor == x11randr::RR_CONFIGURE_PROVIDER_PROPERTY {
            // This request is variable-sized: trailing INT32 values are
            // part of a valid request shape, not an overlong request.
            body.extend_from_slice(&17i32.to_le_bytes());
        }
        let mut state = ServerState::new();
        state.randr.set_providers(vec![randr_provider(10, 0)]);
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(u16::from(minor)),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units: 1 + (body.len() / 4) as u32,
            },
            &body,
        )
        .expect("process known provider property request");
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], x11::error::BAD_IMPLEMENTATION, "minor {minor}");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 0);
    }
}

#[test]
fn randr_change_provider_property_pins_xorg_error_precedence() {
    use yserver_protocol::x11::randr as x11randr;

    let mut cases = Vec::new();
    let mut bad_mode = randr_provider_request_body(x11randr::RR_CHANGE_PROVIDER_PROPERTY, 99);
    bad_mode[13] = 99;
    bad_mode[16..20].copy_from_slice(&1u32.to_le_bytes());
    cases.push((bad_mode, x11::error::BAD_VALUE, 99));
    let mut bad_format = randr_provider_request_body(x11randr::RR_CHANGE_PROVIDER_PROPERTY, 99);
    bad_format[12] = 7;
    bad_format[16..20].copy_from_slice(&1u32.to_le_bytes());
    cases.push((bad_format, x11::error::BAD_VALUE, 7));
    let mut bad_length = randr_provider_request_body(x11randr::RR_CHANGE_PROVIDER_PROPERTY, 99);
    bad_length[16..20].copy_from_slice(&1u32.to_le_bytes());
    cases.push((bad_length, x11::error::BAD_LENGTH, 0));

    for (body, error_code, error_value) in cases {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(39),
            RequestHeader {
                opcode: 128,
                data: x11randr::RR_CHANGE_PROVIDER_PROPERTY,
                length_units: 1 + (body.len() / 4) as u32,
            },
            &body,
        )
        .expect("process malformed ChangeProviderProperty");
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], error_code);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            error_value
        );
    }
}

/// RANDR minors 1 (`RROldGetScreenInfo`) and 3
/// (`RROldScreenChangeSelectInput`) are NULL entries in Xorg's
/// `ProcRandrVector`, and `ProcRRDispatch` treats a NULL slot exactly like
/// an out-of-range minor — `BadRequest`. They previously fell through to
/// the silent "known unsupported" arm and reported success.
#[test]
fn randr_null_dispatch_slots_return_bad_request() {
    for minor in [1u8, 3] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(u16::from(minor)),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units: 1,
            },
            &[],
        )
        .expect("process request");

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32, "minor {minor} must answer");
        assert_eq!(bytes[0], 0, "minor {minor} must be an error, not a reply");
        assert_eq!(bytes[1], x11::error::BAD_REQUEST, "minor {minor} code");
        assert_eq!(&bytes[8..10], &u16::from(minor).to_le_bytes());
        assert_eq!(bytes[10], 128);
    }
}

/// Xorg emits plain `BadValue` here, NOT `BadRRLease`: RRLeaseType is
/// created without `SetResourceTypeErrorValue`, so it keeps dix's default
/// errorValue, and `BadRRLease` is referenced nowhere in the Xorg tree.
/// Expected value measured on real Xorg via `tools/randr-probe`
/// (`RRFreeLease(bogus) -> code=2 (BadValue)`), not derived from this
/// implementation.
#[test]
fn randr_free_lease_without_live_lease_returns_bad_value() {
    const LEASE: u32 = 0x0012_3456;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(46),
        RequestHeader {
            opcode: 128,
            data: 46,
            length_units: 2,
        },
        &LEASE.to_le_bytes(),
    )
    .expect("process FreeLease");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), LEASE);
    assert_eq!(&bytes[8..10], &46u16.to_le_bytes());
    assert_eq!(bytes[10], 128);
}

/// SetScreenConfig resolves its xid as a DRAWABLE, so a PIXMAP is a legal
/// target and must get a normal reply rather than a resource error. Xorg
/// takes `pDraw->pScreen` from whatever drawable it finds (rrscreen.c).
/// Measured on real Xorg with `tools/randr-probe`: a live pixmap returns
/// Success, while yserver used to answer BadWindow.
#[test]
fn randr_set_screen_config_accepts_a_pixmap_drawable() {
    const PIXMAP: u32 = 0x0020_0007;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.resources.create_pixmap(
        ClientId(1),
        x11::CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(PIXMAP),
            drawable: crate::resources::ROOT_WINDOW,
            width: 32,
            height: 32,
        },
    );

    // A client that never sent QueryVersion: the 1.0 request size.
    let mut body = vec![0; 16];
    body[0..4].copy_from_slice(&PIXMAP.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: 2,
            length_units: 5,
        },
        &body,
    )
    .expect("process SetScreenConfig");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32, "expected a 32-byte reply");
    assert_eq!(bytes[0], 1, "must be a reply (1), not an error (0)");
}

#[test]
fn randr_resource_queries_validate_xids_before_replying() {
    const MISSING: u32 = 0x00de_ad01;
    let cases = [
        // SetScreenConfig takes a DRAWABLE (`dixLookupDrawable`), whose
        // BadValue is remapped to BadDrawable — verified on real Xorg with
        // `tools/randr-probe` (bogus xid -> code=9). Every other minor here
        // takes a window and reports BadWindow.
        // 16: the 1.0 size, for a client without QueryVersion.
        (2, 16usize, x11::error::BAD_DRAWABLE),
        (4, 8, x11::error::BAD_WINDOW),
        (5, 4usize, x11::error::BAD_WINDOW),
        (6, 4, x11::error::BAD_WINDOW),
        (7, 16, x11::error::BAD_WINDOW),
        (8, 4, x11::error::BAD_WINDOW),
        (25, 4, x11::error::BAD_WINDOW),
        (31, 4, x11::error::BAD_WINDOW),
        (32, 4, x11::error::BAD_WINDOW),
        (42, 8, x11::error::BAD_WINDOW),
        (9, 8, RANDR_BAD_OUTPUT),
        (10, 4, RANDR_BAD_OUTPUT),
        (11, 8, RANDR_BAD_OUTPUT),
        (15, 24, RANDR_BAD_OUTPUT),
        (20, 8, RANDR_BAD_CRTC),
        (22, 4, RANDR_BAD_CRTC),
        (23, 4, RANDR_BAD_CRTC),
        (24, 8, RANDR_BAD_CRTC),
        (26, 44, RANDR_BAD_CRTC),
        (27, 4, RANDR_BAD_CRTC),
        (28, 4, RANDR_BAD_CRTC),
        (29, 32, RANDR_BAD_CRTC),
    ];

    for (minor, body_len, expected_error) in cases {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let mut backend = RecordingBackend::new();
        let mut body = vec![0; body_len];
        body[0..4].copy_from_slice(&MISSING.to_le_bytes());
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(u16::from(minor)),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units: u32::try_from(1 + body_len / 4).unwrap(),
            },
            &body,
        )
        .expect("process RANDR resource request");

        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32, "minor {minor} must return an error");
        assert_eq!(bytes[1], expected_error, "minor {minor} error");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            MISSING,
            "minor {minor} bad resource",
        );
        assert_eq!(&bytes[8..10], &u16::from(minor).to_le_bytes());
        assert_eq!(bytes[10], 128);
    }
}

/// Xorg treats a primary change as a layout change and fans out
/// ScreenChangeNotify + OutputChangeNotify via `RRTellChanged`
/// (randr/rroutput.c `RRSetPrimaryOutput`). yserver used to just assign the
/// field, so panels never learned the primary moved — they wait for the
/// notify rather than polling GetOutputPrimary. Also pins the idempotent
/// case: re-setting the same output must NOT emit anything.
#[test]
fn randr_set_output_primary_notifies_selecting_clients() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Subscribe to both the screen-change and output-change notifies.
    state.randr_select_masks.insert(
        (1, ROOT_WINDOW),
        x11randr::NOTIFY_MASK_SCREEN_CHANGE | x11randr::NOTIFY_MASK_OUTPUT_CHANGE,
    );
    state.randr.timestamp = 41;
    state.randr.config_timestamp = 37;

    let request = |target: u32| {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&target.to_le_bytes());
        body
    };
    let header = RequestHeader {
        opcode: 128,
        data: x11randr::RR_SET_OUTPUT_PRIMARY,
        length_units: 3,
    };

    // Re-selecting the topology-derived default is wire-idempotent, but
    // records that the client now owns this primary choice.
    assert!(!state.randr_primary_output_explicit);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(0),
        header,
        &request(output),
    )
    .expect("make default primary explicit");
    assert!(state.randr_primary_output_explicit);
    assert!(read_all_available(&mut peer).is_empty());

    // The first output is primary by default, so clear it first to make the
    // set below a real change. (The clear is itself a change and notifies —
    // drained here so the assertions below see only the set.)
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &request(0),
    )
    .expect("clear primary");
    let _ = read_all_available(&mut peer);
    assert_eq!(
        (state.randr.timestamp, state.randr.config_timestamp),
        (41, 37)
    );

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        header,
        &request(output),
    )
    .expect("set primary");

    // Two 32-byte events: ScreenChangeNotify then OutputChangeNotify for
    // the newly-primary output (the previous primary was None, so it
    // contributes nothing).
    let events = read_all_available(&mut peer);
    assert_eq!(events.len(), 64, "expected ScreenChange + OutputChange");
    const RANDR_FIRST_EVENT: u8 = 89;
    assert_eq!(events[0] & 0x7f, RANDR_FIRST_EVENT, "ScreenChangeNotify");
    assert_eq!(
        events[32] & 0x7f,
        RANDR_FIRST_EVENT + 1,
        "RRNotify (OutputChange)"
    );
    for event in events.chunks_exact(32) {
        assert_eq!(u32::from_le_bytes(event[4..8].try_into().unwrap()), 41);
        assert_eq!(u32::from_le_bytes(event[8..12].try_into().unwrap()), 37);
    }
    assert_eq!(
        (state.randr.timestamp, state.randr.config_timestamp),
        (41, 37)
    );

    // Setting the same output again changes nothing, so it must be silent.
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        header,
        &request(output),
    )
    .expect("set same primary");
    assert!(
        read_all_available(&mut peer).is_empty(),
        "idempotent set must not notify",
    );
}

#[test]
fn randr_set_output_primary_updates_get_output_primary() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let request = |output: u32| {
        let mut body = Vec::with_capacity(8);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&output.to_le_bytes());
        body
    };
    let set_header = RequestHeader {
        opcode: 128,
        data: x11randr::RR_SET_OUTPUT_PRIMARY,
        length_units: 3,
    };

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        set_header,
        &request(0),
    )
    .expect("clear primary");
    assert_eq!(state.randr.primary_output, 0);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        set_header,
        &request(output),
    )
    .expect("set primary");
    assert_eq!(state.randr.primary_output, output);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 128,
            data: x11randr::RR_GET_OUTPUT_PRIMARY,
            length_units: 2,
        },
        &ROOT_WINDOW.0.to_le_bytes(),
    )
    .expect("get primary");
    let reply = read_all_available(&mut peer);
    assert_eq!(reply[0], 1);
    assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), output);

    let missing = 0x00de_ad02;
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(4),
        set_header,
        &request(missing),
    )
    .expect("reject unknown primary");
    let error = read_all_available(&mut peer);
    assert_eq!(error[1], RANDR_BAD_OUTPUT);
    assert_eq!(u32::from_le_bytes(error[4..8].try_into().unwrap()), missing);
    assert_eq!(state.randr.primary_output, output);
}

fn change_output_property_body(
    output: u32,
    property: u32,
    prop_type: u32,
    format: u8,
    mode: u8,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&prop_type.to_le_bytes());
    body.push(format);
    body.push(mode);
    body.extend_from_slice(&[0, 0]);
    let n_units = match format {
        8 => data.len(),
        16 => data.len() / 2,
        32 => data.len() / 4,
        _ => 0,
    };
    body.extend_from_slice(&(n_units as u32).to_le_bytes());
    body.extend_from_slice(data);
    body
}

fn change_output_property_header(body_len: usize) -> RequestHeader {
    RequestHeader {
        opcode: 128,
        data: yserver_protocol::x11::randr::RR_CHANGE_OUTPUT_PROPERTY,
        length_units: u32::try_from(1 + body_len / 4).unwrap(),
    }
}

#[test]
fn randr_change_output_property_creates_and_notifies() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state
        .randr_select_masks
        .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_OUTPUT_PROPERTY);

    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);
    let body =
        change_output_property_body(output, property.0, prop_type.0, 32, 0, &42u32.to_le_bytes());

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("ChangeOutputProperty");

    // Void request: no reply bytes, just the notify event.
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32, "expected exactly one OutputPropertyNotify");
    const RANDR_FIRST_EVENT: u8 = 89;
    assert_eq!(bytes[0] & 0x7f, RANDR_FIRST_EVENT + 1, "RRNotify");
    assert_eq!(bytes[1], x11randr::NOTIFY_OUTPUT_PROPERTY);
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), output);
    assert_eq!(
        u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        property.0
    );
    assert_eq!(bytes[20], x11randr::PROPERTY_NEW_VALUE);

    let stored = state
        .randr_output_properties
        .get(&output)
        .and_then(|props| props.iter().find(|(atom, _)| *atom == property))
        .map(|(_, p)| p)
        .expect("property stored");
    assert_eq!(stored.current.as_ref().unwrap().data, 42u32.to_le_bytes());
    assert_eq!(stored.current.as_ref().unwrap().r#type, prop_type);
}

/// #185: an output property write must not move lastSetTime/lastConfigTime
/// (Xorg `rrproperty.c`). muffin treats a lastSetTime that no longer
/// matches its own SetCrtcConfig reply as an external reconfiguration and
/// rebuilds its monitor config (Cinnamon then comes back at 200%).
#[test]
fn randr_change_output_property_leaves_randr_timestamps_alone() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.randr.timestamp = 29_342;
    state.randr.config_timestamp = 29_133;
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);
    for (seq, mode) in [(1, 0u8), (2, 2u8)] {
        let body = change_output_property_body(
            output,
            property.0,
            prop_type.0,
            32,
            mode,
            &42u32.to_le_bytes(),
        );
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(seq),
            change_output_property_header(body.len()),
            &body,
        )
        .expect("ChangeOutputProperty");
    }
    let mut delete = output.to_le_bytes().to_vec();
    delete.extend_from_slice(&property.0.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_DELETE_OUTPUT_PROPERTY,
            length_units: 3,
        },
        &delete,
    )
    .expect("DeleteOutputProperty");
    assert_eq!(
        (state.randr.timestamp, state.randr.config_timestamp),
        (29_342, 29_133)
    );
}

#[test]
fn randr_change_output_property_replace_overwrites() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);

    for value in [1u32, 2u32] {
        let body = change_output_property_body(
            output,
            property.0,
            prop_type.0,
            32,
            0,
            &value.to_le_bytes(),
        );
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            change_output_property_header(body.len()),
            &body,
        )
        .expect("ChangeOutputProperty");
    }
    let _ = read_all_available(&mut peer);

    let stored = &state.randr_output_properties[&output]
        .iter()
        .find(|(atom, _)| *atom == property)
        .unwrap()
        .1;
    assert_eq!(stored.current.as_ref().unwrap().data, 2u32.to_le_bytes());
}

#[test]
fn randr_change_output_property_append_concatenates() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("STRING", false);

    let body = change_output_property_body(output, property.0, prop_type.0, 8, 0, b"hello");
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("replace");
    // mode=2 Append
    let body = change_output_property_body(output, property.0, prop_type.0, 8, 2, b" world");
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("append");
    let _ = read_all_available(&mut peer);

    let stored = &state.randr_output_properties[&output]
        .iter()
        .find(|(atom, _)| *atom == property)
        .unwrap()
        .1;
    assert_eq!(
        stored.current.as_ref().unwrap().data,
        b"hello world".to_vec()
    );
}

#[test]
fn randr_change_output_property_accepts_wire_padded_format8_data() {
    // A real X11 client pads the trailing value array to a 4-byte
    // boundary (here: 2 real bytes + 2 pad bytes). `nUnits` counts real
    // elements only, so the parser must recover exactly "hi", not
    // "hi\0\0".
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PADDED_PROP", false);
    let prop_type = state.atoms.intern("STRING", false);

    let mut body = change_output_property_body(output, property.0, prop_type.0, 8, 0, b"hi");
    body.extend_from_slice(&[0, 0]); // wire padding to a 4-byte boundary
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("padded change");
    let _ = read_all_available(&mut peer);

    let stored = &state.randr_output_properties[&output]
        .iter()
        .find(|(atom, _)| *atom == property)
        .unwrap()
        .1;
    assert_eq!(stored.current.as_ref().unwrap().data, b"hi".to_vec());
}

#[test]
fn randr_change_output_property_append_type_mismatch_is_bad_match() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let type_a = state.atoms.intern("STRING", false);
    let type_b = state.atoms.intern("CARDINAL", false);

    let body = change_output_property_body(output, property.0, type_a.0, 8, 0, b"hi");
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("replace");
    let _ = read_all_available(&mut peer);

    // mode=1 Prepend with a different type -> BadMatch.
    let body = change_output_property_body(output, property.0, type_b.0, 8, 1, b"yo");
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("prepend mismatch");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
}

#[test]
fn randr_change_output_property_invalid_format_is_bad_value() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);
    let body = change_output_property_body(output, property.0, prop_type.0, 7, 0, &[]);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("bad format");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
}

#[test]
fn randr_change_output_property_invalid_mode_is_bad_value() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);
    let body = change_output_property_body(output, property.0, prop_type.0, 32, 9, &[]);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("bad mode");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
}

#[test]
fn randr_change_output_property_unknown_output_is_bad_output() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);
    let missing = 0x00de_ad01;
    let body = change_output_property_body(missing, property.0, prop_type.0, 32, 0, &[0; 4]);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("bad output");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], RANDR_BAD_OUTPUT);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), missing);
}

#[test]
fn randr_change_output_property_unknown_atom_is_bad_atom() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let bogus_atom = 0x00de_ad01;
    let prop_type = state.atoms.intern("CARDINAL", false);
    let body = change_output_property_body(output, bogus_atom, prop_type.0, 32, 0, &[0; 4]);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("bad atom");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_ATOM);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        bogus_atom
    );
}

fn configure_output_property_body(
    output: u32,
    property: u32,
    pending: bool,
    range: bool,
    values: &[i32],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.to_le_bytes());
    body.push(u8::from(pending));
    body.push(u8::from(range));
    body.extend_from_slice(&[0, 0]);
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

fn configure_output_property_header(body_len: usize) -> RequestHeader {
    RequestHeader {
        opcode: 128,
        data: yserver_protocol::x11::randr::RR_CONFIGURE_OUTPUT_PROPERTY,
        length_units: u32::try_from(1 + body_len / 4).unwrap(),
    }
}

#[test]
fn randr_configure_output_property_stores_range_and_fires_no_notify() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state
        .randr_select_masks
        .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_OUTPUT_PROPERTY);
    let property = state.atoms.intern("TEST_RANGE_PROP", false);

    let body = configure_output_property_body(output, property.0, false, true, &[0, 100]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        configure_output_property_header(body.len()),
        &body,
    )
    .expect("ConfigureOutputProperty");

    assert!(
        read_all_available(&mut peer).is_empty(),
        "Configure must not notify"
    );
    let stored = &state.randr_output_properties[&output]
        .iter()
        .find(|(atom, _)| *atom == property)
        .unwrap()
        .1;
    assert!(stored.range);
    assert!(!stored.is_pending);
    assert!(!stored.immutable);
    assert_eq!(stored.valid_values, vec![0, 100]);
}

#[test]
fn randr_configure_output_property_odd_range_values_is_bad_match() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_RANGE_PROP", false);

    let body = configure_output_property_body(output, property.0, false, true, &[0, 50, 100]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        configure_output_property_header(body.len()),
        &body,
    )
    .expect("odd range values");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
}

#[test]
fn randr_configure_output_property_leaving_pending_clears_pending_value() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state
        .randr_select_masks
        .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_OUTPUT_PROPERTY);
    let property = state.atoms.intern("TEST_PENDING_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);

    // Mark the property pending-capable, then Change it (lands in
    // `.pending`, but Xorg's `sendevent` is unconditional in
    // `RRChangeOutputProperty` — only the `RRNoticePropertyChange`
    // driver hook (unrelated to client notification) is gated on
    // `is_pending`, so the wire notify still fires here).
    let body = configure_output_property_body(output, property.0, true, false, &[]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        configure_output_property_header(body.len()),
        &body,
    )
    .expect("configure pending");
    let body =
        change_output_property_body(output, property.0, prop_type.0, 32, 0, &7u32.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("change pending value");
    let notify = read_all_available(&mut peer);
    assert_eq!(
        notify.len(),
        32,
        "ChangeOutputProperty must notify even when staged as pending"
    );
    assert_eq!(notify[20], x11randr::PROPERTY_NEW_VALUE);
    assert!(
        state.randr_output_properties[&output]
            .iter()
            .find(|(atom, _)| *atom == property)
            .unwrap()
            .1
            .pending
            .is_some(),
        "pending value must be staged"
    );

    // Configure(pending=false) drops the staged pending value (Xorg:
    // "Property moving from pending to non-pending loses any pending
    // values").
    let body = configure_output_property_body(output, property.0, false, false, &[]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        configure_output_property_header(body.len()),
        &body,
    )
    .expect("configure non-pending");
    let _ = read_all_available(&mut peer);
    let stored = &state.randr_output_properties[&output]
        .iter()
        .find(|(atom, _)| *atom == property)
        .unwrap()
        .1;
    assert!(!stored.is_pending);
    assert!(stored.pending.is_none());
}

#[test]
fn randr_configure_output_property_unknown_output_is_bad_output() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_RANGE_PROP", false);
    let missing = 0x00de_ad01;

    let body = configure_output_property_body(missing, property.0, false, false, &[]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        configure_output_property_header(body.len()),
        &body,
    )
    .expect("bad output");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], RANDR_BAD_OUTPUT);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), missing);
}

fn delete_output_property_header() -> RequestHeader {
    RequestHeader {
        opcode: 128,
        data: yserver_protocol::x11::randr::RR_DELETE_OUTPUT_PROPERTY,
        length_units: 3,
    }
}

#[test]
fn randr_delete_output_property_removes_and_notifies() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);

    let body =
        change_output_property_body(output, property.0, prop_type.0, 32, 0, &1u32.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("create property");
    let _ = read_all_available(&mut peer);
    state
        .randr_select_masks
        .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_OUTPUT_PROPERTY);

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        delete_output_property_header(),
        &body,
    )
    .expect("DeleteOutputProperty");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    const RANDR_FIRST_EVENT: u8 = 89;
    assert_eq!(bytes[0] & 0x7f, RANDR_FIRST_EVENT + 1);
    assert_eq!(bytes[1], x11randr::NOTIFY_OUTPUT_PROPERTY);
    assert_eq!(bytes[20], x11randr::PROPERTY_DELETE);
    assert!(
        !state.randr_output_properties[&output]
            .iter()
            .any(|(atom, _)| *atom == property),
        "property must be removed"
    );
}

#[test]
fn randr_delete_output_property_unknown_is_bad_name() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let bogus_atom = state.atoms.intern("NEVER_SET_PROP", false);

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&bogus_atom.0.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        delete_output_property_header(),
        &body,
    )
    .expect("delete unknown");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_NAME);
}

#[test]
fn randr_delete_output_property_unregistered_atom_is_bad_atom() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let never_interned: u32 = 0x00de_ad02;

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&never_interned.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        delete_output_property_header(),
        &body,
    )
    .expect("delete unregistered atom");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_ATOM);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        never_interned
    );
}

#[test]
fn randr_delete_output_property_immutable_is_bad_access() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("IMMUTABLE_PROP", false);
    state
        .randr_output_properties
        .entry(output)
        .or_default()
        .push((
            property,
            crate::randr::RandrOutputProperty {
                immutable: true,
                ..Default::default()
            },
        ));

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        delete_output_property_header(),
        &body,
    )
    .expect("delete immutable");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_ACCESS);
    assert!(
        state.randr_output_properties[&output]
            .iter()
            .any(|(atom, _)| *atom == property),
        "immutable property must not be removed"
    );
}

#[test]
fn randr_delete_output_property_unknown_output_is_bad_output() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let missing: u32 = 0x00de_ad01;

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&missing.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        delete_output_property_header(),
        &body,
    )
    .expect("bad output");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], RANDR_BAD_OUTPUT);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), missing);
}

#[test]
fn randr_query_output_property_reports_stored_metadata() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_RANGE_PROP", false);

    let body = configure_output_property_body(output, property.0, false, true, &[0, 100]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        configure_output_property_header(body.len()),
        &body,
    )
    .expect("configure");

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_QUERY_OUTPUT_PROPERTY,
            length_units: 3,
        },
        &body,
    )
    .expect("query");
    let reply = read_all_available(&mut peer);
    assert_eq!(reply[0], 1, "must be a reply");
    assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 2); // length = num_valid
    assert_eq!(reply[8], 0, "pending");
    assert_eq!(reply[9], 1, "range");
    assert_eq!(reply[10], 0, "immutable");
    assert_eq!(i32::from_le_bytes(reply[32..36].try_into().unwrap()), 0);
    assert_eq!(i32::from_le_bytes(reply[36..40].try_into().unwrap()), 100);
}

#[test]
fn randr_query_output_property_unknown_atom_is_bad_name() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let bogus_atom = state.atoms.intern("NEVER_CONFIGURED_PROP", false);

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&bogus_atom.0.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_QUERY_OUTPUT_PROPERTY,
            length_units: 3,
        },
        &body,
    )
    .expect("query unknown");
    let reply = read_all_available(&mut peer);
    assert_eq!(reply.len(), 32);
    assert_eq!(reply[1], x11::error::BAD_NAME);
}

#[test]
fn randr_get_output_property_round_trips_changed_value() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);

    let body =
        change_output_property_body(output, property.0, prop_type.0, 32, 0, &99u32.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("change");
    let _ = read_all_available(&mut peer);

    // RRGetOutputProperty(output, property, AnyPropertyType, 0, 1, false, false)
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
    body.extend_from_slice(&0u32.to_le_bytes()); // long_offset
    body.extend_from_slice(&1u32.to_le_bytes()); // long_length
    body.push(0); // delete
    body.push(0); // pending
    body.extend_from_slice(&[0, 0]); // pad
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_OUTPUT_PROPERTY,
            length_units: 6,
        },
        &body,
    )
    .expect("get");
    let reply = read_all_available(&mut peer);
    assert_eq!(reply[0], 1);
    assert_eq!(reply[1], 32, "format");
    assert_eq!(
        u32::from_le_bytes(reply[8..12].try_into().unwrap()),
        prop_type.0
    );
    assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0); // bytes_after
    assert_eq!(u32::from_le_bytes(reply[16..20].try_into().unwrap()), 1); // nItems
    assert_eq!(&reply[32..36], &99u32.to_le_bytes());
}

#[test]
fn randr_get_output_property_type_mismatch_returns_metadata_only() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let real_type = state.atoms.intern("CARDINAL", false);
    let other_type = state.atoms.intern("STRING", false);

    // Two format-32 elements (8 bytes) so byte-count vs element-count
    // diverge: Xorg's `bytesAfter` for a type mismatch is
    // `prop_value->size`, an ELEMENT count (`rrproperty.c`
    // `RRChangeOutputProperty`: `new_value.size = total_len` where
    // `total_len` is `nUnits`, not a byte length), not `full.len()`.
    let mut value = Vec::new();
    value.extend_from_slice(&1u32.to_le_bytes());
    value.extend_from_slice(&2u32.to_le_bytes());
    let body = change_output_property_body(output, property.0, real_type.0, 32, 0, &value);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("change");
    let _ = read_all_available(&mut peer);

    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&other_type.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(0);
    body.push(0);
    body.extend_from_slice(&[0, 0]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_OUTPUT_PROPERTY,
            length_units: 6,
        },
        &body,
    )
    .expect("get mismatched type");
    let reply = read_all_available(&mut peer);
    assert_eq!(
        u32::from_le_bytes(reply[8..12].try_into().unwrap()),
        real_type.0,
        "propertyType must be the real type"
    );
    assert_eq!(
        u32::from_le_bytes(reply[12..16].try_into().unwrap()),
        2,
        "bytes_after is an ELEMENT count (2 x format-32), not a byte count (8)"
    );
    assert_eq!(u32::from_le_bytes(reply[16..20].try_into().unwrap()), 0); // nItems
}

#[test]
fn randr_get_output_property_unregistered_property_atom_is_bad_atom() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let never_interned: u32 = 0x00de_ad03;

    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&never_interned.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_OUTPUT_PROPERTY,
            length_units: 6,
        },
        &body,
    )
    .expect("get unregistered property atom");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_ATOM);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        never_interned
    );
}

#[test]
fn randr_get_output_property_unregistered_type_atom_is_bad_atom() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let never_interned: u32 = 0x00de_ad04;

    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&never_interned.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_OUTPUT_PROPERTY,
            length_units: 6,
        },
        &body,
    )
    .expect("get unregistered type atom");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_ATOM);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        never_interned
    );
}

#[test]
fn randr_get_output_property_invalid_delete_byte_is_bad_value() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);

    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(2); // delete: neither xTrue(1) nor xFalse(0)
    body.push(0);
    body.extend_from_slice(&[0, 0]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_OUTPUT_PROPERTY,
            length_units: 6,
        },
        &body,
    )
    .expect("get invalid delete byte");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
}

#[test]
fn randr_get_output_property_delete_removes_and_notifies() {
    use yserver_protocol::x11::randr as x11randr;

    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);

    let body =
        change_output_property_body(output, property.0, prop_type.0, 32, 0, &1u32.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("change");
    let _ = read_all_available(&mut peer);
    state
        .randr_select_masks
        .insert((1, ROOT_WINDOW), x11randr::NOTIFY_MASK_OUTPUT_PROPERTY);

    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&output.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
    body.extend_from_slice(&0u32.to_le_bytes()); // long_offset
    body.extend_from_slice(&1u32.to_le_bytes()); // long_length
    body.push(1); // delete = true
    body.push(0);
    body.extend_from_slice(&[0, 0]);
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_GET_OUTPUT_PROPERTY,
            length_units: 6,
        },
        &body,
    )
    .expect("get with delete");

    let bytes = read_all_available(&mut peer);
    // One 32-byte OutputPropertyNotify(state=Delete) followed by the
    // 36-byte reply (32-byte header + the just-deleted 4-byte value:
    // Xorg reads the value before removing the property).
    assert_eq!(bytes.len(), 68);
    const RANDR_FIRST_EVENT: u8 = 89;
    assert_eq!(bytes[0] & 0x7f, RANDR_FIRST_EVENT + 1, "RRNotify");
    assert_eq!(bytes[20], x11randr::PROPERTY_DELETE);
    assert_eq!(bytes[32], 1, "reply type");
    assert!(
        !state.randr_output_properties[&output]
            .iter()
            .any(|(atom, _)| *atom == property),
        "property must be removed after delete=true read"
    );
}

#[test]
fn randr_list_output_properties_includes_stored_atoms() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let property = state.atoms.intern("TEST_PROP", false);
    let prop_type = state.atoms.intern("CARDINAL", false);

    let body =
        change_output_property_body(output, property.0, prop_type.0, 32, 0, &1u32.to_le_bytes());
    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        change_output_property_header(body.len()),
        &body,
    )
    .expect("change");
    let _ = read_all_available(&mut peer);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_LIST_OUTPUT_PROPERTIES,
            length_units: 2,
        },
        &output.to_le_bytes(),
    )
    .expect("list");
    let reply = read_all_available(&mut peer);
    assert_eq!(reply[0], 1);
    let n_atoms = u16::from_le_bytes(reply[8..10].try_into().unwrap());
    assert_eq!(n_atoms, 1);
    assert_eq!(
        u32::from_le_bytes(reply[32..36].try_into().unwrap()),
        property.0
    );
}

#[test]
fn randr_list_output_properties_orders_newest_first() {
    let mut state = ServerState::new();
    let output = state.randr.outputs[0].output_id;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let prop_type = state.atoms.intern("CARDINAL", false);
    let first = state.atoms.intern("FIRST_PROP", false);
    let second = state.atoms.intern("SECOND_PROP", false);

    for property in [first, second] {
        let body = change_output_property_body(
            output,
            property.0,
            prop_type.0,
            32,
            0,
            &1u32.to_le_bytes(),
        );
        handle_randr_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            change_output_property_header(body.len()),
            &body,
        )
        .expect("change");
    }
    let _ = read_all_available(&mut peer);

    handle_randr_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 128,
            data: yserver_protocol::x11::randr::RR_LIST_OUTPUT_PROPERTIES,
            length_units: 2,
        },
        &output.to_le_bytes(),
    )
    .expect("list");
    let reply = read_all_available(&mut peer);
    let n_atoms = u16::from_le_bytes(reply[8..10].try_into().unwrap());
    assert_eq!(n_atoms, 2);
    // Xorg's RRCreateOutputProperty prepends onto a linked list, so
    // ListOutputProperties enumerates newest-first.
    assert_eq!(
        u32::from_le_bytes(reply[32..36].try_into().unwrap()),
        second.0,
        "most recently created property must list first"
    );
    assert_eq!(
        u32::from_le_bytes(reply[36..40].try_into().unwrap()),
        first.0
    );
}
