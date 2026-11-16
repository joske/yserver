use super::{
    access_notify_support::*,
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject},
    backend::recording::{RecordedCall, RecordingBackend},
    properties::{PropertyFormat, PropertyValue},
    resources::{ROOT_COLORMAP, ROOT_WINDOW, SERVER_OWNER},
    rootless::{self, CoordinatePolicy, HostRequest, RootlessEvent, SelectionEvent},
    server::CoreFocus,
};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use yserver_protocol::x11::{AtomId, CreateWindowRequest};

const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};
const FOCUS_MASK: u32 = 1 << 21;
const COLORMAP_MASK: u32 = 1 << 23;
const WINDOW_A: ResourceId = ResourceId(0x0055_0010);
const WINDOW_B: ResourceId = ResourceId(0x0055_0011);
const FOCUS_OLD_TOP: ResourceId = ResourceId(0x0055_0020);
const FOCUS_OLD_CHILD: ResourceId = ResourceId(0x0055_0021);
const FOCUS_NEW_TOP: ResourceId = ResourceId(0x0055_0022);
const FOCUS_NEW_CHILD: ResourceId = ResourceId(0x0055_0023);
const HOST_TOP: ResourceId = ResourceId(0x0055_0030);
const TEST_COLORMAP: ResourceId = ResourceId(0x0055_0040);

fn receives(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

fn wire_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let event_type = bytes[offset] & 0x7f;
        let length = if event_type == 35 {
            let words = u32::from_le_bytes(
                bytes[offset + 4..offset + 8]
                    .try_into()
                    .expect("generic event length"),
            );
            32 + usize::try_from(words).unwrap() * 4
        } else {
            32
        };
        assert!(offset + length <= bytes.len(), "complete protocol frame");
        frames.push(bytes[offset..offset + length].to_vec());
        offset += length;
    }
    frames
}

fn focus_fields(bytes: &[u8]) -> Vec<(u8, u8, ResourceId, u8)> {
    wire_frames(bytes)
        .into_iter()
        .filter(|frame| matches!(frame[0] & 0x7f, 9 | 10))
        .map(|frame| {
            (
                frame[0] & 0x7f,
                frame[1],
                ResourceId(u32::from_le_bytes(frame[4..8].try_into().unwrap())),
                frame[8],
            )
        })
        .collect()
}

fn focus_state(state: &ServerState) -> (u32, u8, u32) {
    (
        state.core_focus.raw,
        state.core_focus.revert_to,
        state.core_focus.time,
    )
}

fn keyboard_grab_state(state: &ServerState) -> Option<(ClientId, ResourceId, bool, bool, u32)> {
    state.active_keyboard_grab.map(|grab| {
        (
            grab.owner,
            grab.grab_window,
            grab.owner_events,
            grab.via_xi2,
            grab.xi2_mask,
        )
    })
}

fn request(
    fixture: &mut NotifyFixture,
    client: ClientId,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> RequestOutcome {
    fixture
        .request(
            client,
            SequenceNumber(sequence),
            RequestHeader {
                opcode,
                data,
                length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
            },
            body,
        )
        .expect("request through process_request")
}

fn create_window(
    state: &mut ServerState,
    id: ResourceId,
    parent: ResourceId,
    colormap: Option<ResourceId>,
) {
    state.resources.create_window(
        NOTIFY_SENDER,
        CreateWindowRequest {
            depth: 24,
            window: id,
            parent,
            width: 120,
            height: 90,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            colormap: colormap.map(Some),
            ..Default::default()
        },
    );
}

fn access_notify_focus_fixture() -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    for (window, parent) in [
        (FOCUS_OLD_TOP, ROOT_WINDOW),
        (FOCUS_OLD_CHILD, FOCUS_OLD_TOP),
        (FOCUS_NEW_TOP, ROOT_WINDOW),
        (FOCUS_NEW_CHILD, FOCUS_NEW_TOP),
    ] {
        create_window(&mut fixture.state, window, parent, None);
        assert!(fixture.state.resources.map_window(window).mapping_changed);
    }
    fixture.state.core_focus = CoreFocus {
        raw: FOCUS_OLD_CHILD.0,
        revert_to: 2,
        time: 0,
    };
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let client_state = fixture.state.clients.get_mut(&client.0).expect("receiver");
        for window in [
            FOCUS_OLD_TOP,
            FOCUS_OLD_CHILD,
            FOCUS_NEW_TOP,
            FOCUS_NEW_CHILD,
        ] {
            client_state.event_masks.insert(window, FOCUS_MASK);
        }
    }
    let denied = fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .expect("denied receiver");
    denied
        .xi2_masks
        .insert((FOCUS_OLD_CHILD, 3), (1 << 9) | (1 << 10));
    denied
        .xi2_masks
        .insert((FOCUS_NEW_CHILD, 3), (1 << 9) | (1 << 10));
    fixture
}

fn focus_policy(
    fixture: &mut NotifyFixture,
    deny: bool,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    fixture.install_policy(move |_, call| {
        let denied_event = call
            .events
            .as_ref()
            .and_then(|events| events.first())
            .map(|event| event.type_code);
        if deny
            && call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && matches!(call.receive, Some(EventSubject::Window(window)) if
                (call.request == request_opcodes(42, 0) && window == FOCUS_OLD_TOP)
                    || (call.request == request_opcodes(31, 0)
                        && window == FOCUS_NEW_CHILD && denied_event == Some(10))
                    || (call.request == request_opcodes(32, 0)
                        && window == FOCUS_NEW_CHILD && denied_event == Some(10))
                    || (call.request == request_opcodes(4, 0)
                        && window == FOCUS_NEW_CHILD && denied_event == Some(10)))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    })
}

fn set_focus_to_new_child(fixture: &mut NotifyFixture) {
    let mut body = FOCUS_NEW_CHILD.0.to_le_bytes().to_vec();
    body.extend_from_slice(&0_u32.to_le_bytes());
    assert!(matches!(
        request(fixture, NOTIFY_SENDER, 41, 42, 2, &body),
        RequestOutcome::Handled
    ));
}

fn clear_focus_streams(fixture: &mut NotifyFixture) -> Vec<(ClientId, Vec<u8>)> {
    [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ]
    .into_iter()
    .map(|client| (client, fixture.drain(client)))
    .collect()
}

fn stream(streams: &[(ClientId, Vec<u8>)], client: ClientId) -> &[u8] {
    &streams
        .iter()
        .find(|(id, _)| *id == client)
        .expect("client stream")
        .1
}

fn focus_grab(fixture: &mut NotifyFixture, opcode: u8, sequence: u16) {
    if opcode == 31 {
        let mut body = FOCUS_NEW_CHILD.0.to_le_bytes().to_vec();
        body.extend_from_slice(&[0; 8]);
        assert!(matches!(
            request(fixture, NOTIFY_SENDER, sequence, opcode, 0, &body),
            RequestOutcome::Handled
        ));
    } else {
        assert!(matches!(
            request(fixture, NOTIFY_SENDER, sequence, opcode, 0, &[0; 4]),
            RequestOutcome::Handled
        ));
    }
}

fn filter_event_window(bytes: &[u8], event_type: u8, window: ResourceId) -> Vec<u8> {
    wire_frames(bytes)
        .into_iter()
        .filter(|frame| {
            frame[0] & 0x7f != event_type
                || ResourceId(u32::from_le_bytes(frame[4..8].try_into().unwrap())) != window
        })
        .flatten()
        .collect()
}

fn assert_only_focus_event_removed(
    control: &[u8],
    denied: &[u8],
    window: ResourceId,
    event_type: u8,
) {
    assert_eq!(
        wire_frames(denied),
        wire_frames(&filter_event_window(control, event_type, window))
    );
}

// Mutations: change a no-policy focus field in production, use the final focus
// for every subject, suppress XI2 with core denial, or skip NotifyGrab/Ungrab
// and server-mode checks.
#[test]
fn receive_focus_checks_transition_windows_and_keeps_xi2() {
    let mut baseline = access_notify_focus_fixture();
    assert!(!baseline.state.policies_installed());
    set_focus_to_new_child(&mut baseline);
    let baseline_focus = clear_focus_streams(&mut baseline);
    assert_eq!(
        focus_fields(stream(&baseline_focus, NOTIFY_RECEIVER_ALLOWED)),
        [
            (10, 3, FOCUS_OLD_CHILD, 0),
            (10, 4, FOCUS_OLD_TOP, 0),
            (9, 4, FOCUS_NEW_TOP, 0),
            (9, 3, FOCUS_NEW_CHILD, 0),
        ],
        "no-policy focus transition fields"
    );

    let mut control = access_notify_focus_fixture();
    let control_calls = focus_policy(&mut control, false);
    set_focus_to_new_child(&mut control);
    let control_focus = clear_focus_streams(&mut control);
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
        assert_eq!(
            stream(&control_focus, client),
            stream(&baseline_focus, client),
            "focus transition no-policy bytes match recording-Allow"
        );
    }
    assert_eq!(
        focus_fields(stream(&control_focus, NOTIFY_RECEIVER_ALLOWED)),
        [
            (10, 3, FOCUS_OLD_CHILD, 0),
            (10, 4, FOCUS_OLD_TOP, 0),
            (9, 4, FOCUS_NEW_TOP, 0),
            (9, 3, FOCUS_NEW_CHILD, 0),
        ]
    );
    let xi2: Vec<_> = wire_frames(stream(&control_focus, NOTIFY_RECEIVER_DENIED))
        .into_iter()
        .filter(|frame| frame[0] & 0x7f == 35)
        .collect();
    assert_eq!(xi2.len(), 2, "XI2 FocusOut and FocusIn remain selected");
    assert_eq!(
        xi2.iter()
            .map(|frame| u16::from_le_bytes(frame[8..10].try_into().unwrap()))
            .collect::<Vec<_>>(),
        [10, 9]
    );

    let mut denied = access_notify_focus_fixture();
    let denied_calls = focus_policy(&mut denied, true);
    set_focus_to_new_child(&mut denied);
    let denied_focus = clear_focus_streams(&mut denied);
    assert_only_focus_event_removed(
        stream(&control_focus, NOTIFY_RECEIVER_DENIED),
        stream(&denied_focus, NOTIFY_RECEIVER_DENIED),
        FOCUS_OLD_TOP,
        10,
    );
    assert_eq!(
        stream(&control_focus, NOTIFY_RECEIVER_ALLOWED),
        stream(&denied_focus, NOTIFY_RECEIVER_ALLOWED)
    );
    assert_eq!(
        focus_fields(stream(&denied_focus, NOTIFY_RECEIVER_DENIED)),
        [
            (10, 3, FOCUS_OLD_CHILD, 0),
            (9, 4, FOCUS_NEW_TOP, 0),
            (9, 3, FOCUS_NEW_CHILD, 0),
        ]
    );
    assert_eq!(focus_state(&control.state), focus_state(&denied.state));
    assert_eq!(
        control
            .state
            .clients
            .iter()
            .map(|(id, client)| (*id, client.focused_window))
            .collect::<HashSet<_>>(),
        denied
            .state
            .clients
            .iter()
            .map(|(id, client)| (*id, client.focused_window))
            .collect::<HashSet<_>>()
    );

    for (opcode, sequence, expected_mode) in [(31, 42, 1), (32, 43, 2)] {
        focus_grab(&mut control, opcode, sequence);
        let control_streams = clear_focus_streams(&mut control);
        focus_grab(&mut baseline, opcode, sequence);
        let baseline_streams = clear_focus_streams(&mut baseline);
        for client in [
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ] {
            assert_eq!(
                stream(&control_streams, client),
                stream(&baseline_streams, client),
                "grab transition no-policy bytes match recording-Allow"
            );
        }
        focus_grab(&mut denied, opcode, sequence);
        let denied_streams = clear_focus_streams(&mut denied);
        assert_only_focus_event_removed(
            stream(&control_streams, NOTIFY_RECEIVER_DENIED),
            stream(&denied_streams, NOTIFY_RECEIVER_DENIED),
            FOCUS_NEW_CHILD,
            10,
        );
        assert_eq!(
            stream(&control_streams, NOTIFY_SENDER),
            stream(&denied_streams, NOTIFY_SENDER),
            "GrabKeyboard's reply and UngrabKeyboard output are isolated"
        );
        assert_eq!(
            stream(&control_streams, NOTIFY_RECEIVER_ALLOWED),
            stream(&denied_streams, NOTIFY_RECEIVER_ALLOWED)
        );
        let focus = focus_fields(stream(&control_streams, NOTIFY_RECEIVER_ALLOWED));
        assert_eq!(focus.len(), 2);
        assert!(focus.iter().all(|event| event.3 == expected_mode));
        assert_eq!(
            keyboard_grab_state(&control.state),
            keyboard_grab_state(&denied.state)
        );
        assert_eq!(focus_state(&control.state), focus_state(&denied.state));
    }

    let destroy_body = FOCUS_NEW_CHILD.0.to_le_bytes();
    assert!(matches!(
        request(&mut control, NOTIFY_SENDER, 44, 4, 0, &destroy_body),
        RequestOutcome::Handled
    ));
    let control_destroy = clear_focus_streams(&mut control);
    assert!(matches!(
        request(&mut baseline, NOTIFY_SENDER, 44, 4, 0, &destroy_body),
        RequestOutcome::Handled
    ));
    let baseline_destroy = clear_focus_streams(&mut baseline);
    assert_eq!(
        focus_fields(stream(&baseline_destroy, NOTIFY_RECEIVER_ALLOWED)),
        [(10, 0, FOCUS_NEW_CHILD, 0), (9, 2, FOCUS_NEW_TOP, 0)],
        "no-policy DestroyWindow focus events"
    );
    for client in [
        NOTIFY_SENDER,
        NOTIFY_RECEIVER_DENIED,
        NOTIFY_RECEIVER_ALLOWED,
    ] {
        assert_eq!(
            stream(&control_destroy, client),
            stream(&baseline_destroy, client),
            "destroy focus no-policy bytes match recording-Allow"
        );
    }
    assert!(matches!(
        request(&mut denied, NOTIFY_SENDER, 44, 4, 0, &destroy_body),
        RequestOutcome::Handled
    ));
    let denied_destroy = clear_focus_streams(&mut denied);
    assert_only_focus_event_removed(
        stream(&control_destroy, NOTIFY_RECEIVER_DENIED),
        stream(&denied_destroy, NOTIFY_RECEIVER_DENIED),
        FOCUS_NEW_CHILD,
        10,
    );
    assert_eq!(
        focus_fields(stream(&control_destroy, NOTIFY_RECEIVER_ALLOWED)),
        [(10, 0, FOCUS_NEW_CHILD, 0), (9, 2, FOCUS_NEW_TOP, 0)]
    );
    assert_eq!(
        stream(&control_destroy, NOTIFY_RECEIVER_ALLOWED),
        stream(&denied_destroy, NOTIFY_RECEIVER_ALLOWED)
    );
    assert_eq!(focus_state(&control.state), focus_state(&denied.state));
    assert_eq!(control.state.core_focus.raw, FOCUS_NEW_TOP.0);
    assert!(control.state.resources.window(FOCUS_NEW_CHILD).is_none());
    assert!(denied.state.resources.window(FOCUS_NEW_CHILD).is_none());

    for calls in [control_calls, denied_calls] {
        let calls = calls.lock().unwrap();
        let focus_receives = receives(&calls)
            .into_iter()
            .filter(|call| {
                call.events.as_ref().is_some_and(|events| {
                    events.iter().all(|event| matches!(event.type_code, 9 | 10))
                })
            })
            .collect::<Vec<_>>();
        assert!(focus_receives.iter().any(|call| {
            call.receive == Some(EventSubject::Window(FOCUS_OLD_TOP))
                && call.events == Some(vec![EventDescription::core(10)])
                && call.request == request_opcodes(42, 0)
        }));
        assert!(focus_receives.iter().any(|call| {
            call.receive == Some(EventSubject::Window(FOCUS_NEW_CHILD))
                && call.events == Some(vec![EventDescription::core(9)])
                && call.request == request_opcodes(42, 0)
        }));
        assert!(focus_receives.iter().any(|call| {
            call.receive == Some(EventSubject::Window(FOCUS_NEW_CHILD))
                && call.request == request_opcodes(31, 0)
        }));
        assert!(focus_receives.iter().any(|call| {
            call.receive == Some(EventSubject::Window(FOCUS_NEW_CHILD))
                && call.request == request_opcodes(32, 0)
        }));
        assert!(focus_receives.iter().any(|call| {
            call.receive == Some(EventSubject::Window(FOCUS_NEW_CHILD))
                && call.request == request_opcodes(4, 0)
        }));
        assert_eq!(
            focus_receives.len(),
            20,
            "every selected core focus delivery is checked"
        );
        let mut actual: Vec<_> = focus_receives
            .iter()
            .map(|call| {
                (
                    call.client.0,
                    call.request.expect("request-scoped focus check").major,
                    match call.receive.expect("focus subject") {
                        EventSubject::Window(window) => window.0,
                        other => panic!("unexpected focus subject: {other:?}"),
                    },
                    call.events.as_ref().unwrap()[0].type_code,
                )
            })
            .collect();
        actual.sort_unstable();
        let mut expected = Vec::new();
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            for (opcode, window, event_type) in [
                (42, FOCUS_OLD_CHILD, 10),
                (42, FOCUS_OLD_TOP, 10),
                (42, FOCUS_NEW_TOP, 9),
                (42, FOCUS_NEW_CHILD, 9),
                (31, FOCUS_NEW_CHILD, 10),
                (31, FOCUS_NEW_CHILD, 9),
                (32, FOCUS_NEW_CHILD, 10),
                (32, FOCUS_NEW_CHILD, 9),
                (4, FOCUS_NEW_CHILD, 10),
                (4, FOCUS_NEW_TOP, 9),
            ] {
                expected.push((client.0, opcode, window.0, event_type));
            }
        }
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    // Host activation is independent server work: it reaches the real host entry point
    // with a live mapped top-level and carries no stale request opcode.
    let host_fixture = || {
        let mut host = NotifyFixture::new();
        host.backend = RecordingBackend::new().with_redirect_activation();
        rootless::enable(&mut host.state, &mut host.backend, CoordinatePolicy::Opaque)
            .expect("enable rootless");
        create_window(&mut host.state, HOST_TOP, ROOT_WINDOW, None);
        let _ = host.state.resources.map_window(HOST_TOP);
        let id = host
            .backend
            .calls()
            .iter()
            .flat_map(|call| match call {
                RecordedCall::RootlessEvents(events) => events.iter().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .find_map(|event| match event {
                RootlessEvent::Mapped { id, window, .. } if *window == HOST_TOP => Some(*id),
                _ => None,
            });
        if id.is_none() {
            rootless::reconcile_and_dispatch(&mut host.state, &mut host.backend);
        }
        let id = id
            .or_else(|| {
                host.backend
                    .calls()
                    .iter()
                    .flat_map(|call| match call {
                        RecordedCall::RootlessEvents(events) => events.iter().collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .find_map(|event| match event {
                        RootlessEvent::Mapped { id, window, .. } if *window == HOST_TOP => {
                            Some(*id)
                        }
                        _ => None,
                    })
            })
            .expect("reconciled live top-level id");
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            host.state
                .clients
                .get_mut(&client.0)
                .unwrap()
                .event_masks
                .insert(HOST_TOP, FOCUS_MASK);
        }
        host.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
        (host, id)
    };

    let (mut host_baseline, host_baseline_id) = host_fixture();
    assert!(!host_baseline.state.policies_installed());
    rootless::host_request(
        &mut host_baseline.state,
        &mut host_baseline.backend,
        HostRequest::Activated {
            id: host_baseline_id,
        },
    );
    let host_baseline_denied = host_baseline.drain(NOTIFY_RECEIVER_DENIED);
    let host_baseline_allowed = host_baseline.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        focus_fields(&host_baseline_allowed),
        [(10, 5, HOST_TOP, 0), (9, 3, HOST_TOP, 0)],
        "no-policy host activation focus fields"
    );

    let (mut host, id) = host_fixture();
    let host_calls = host.install_policy(|_, _| Decision::Allow);
    rootless::host_request(
        &mut host.state,
        &mut host.backend,
        HostRequest::Activated { id },
    );
    assert_eq!(host.state.core_focus.raw, HOST_TOP.0);
    assert_eq!(host.drain(NOTIFY_RECEIVER_DENIED), host_baseline_denied);
    let host_allowed = host.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(host_allowed, host_baseline_allowed);
    assert!(receives(&host_calls.lock().unwrap()).iter().any(|call| {
        call.request.is_none()
            && call.receive == Some(EventSubject::Window(HOST_TOP))
            && call.events == Some(vec![EventDescription::core(9)])
    }));
    let host_recorded = host_calls.lock().unwrap();
    let host_receive_checks = receives(&host_recorded);
    assert_eq!(host_receive_checks.len(), 4, "{host_receive_checks:#?}");
    assert_eq!(
        focus_fields(&host_allowed),
        [(10, 5, HOST_TOP, 0), (9, 3, HOST_TOP, 0)]
    );
}

fn colormap_fixture() -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    fixture.state.resources.create_colormap(
        NOTIFY_SENDER,
        TEST_COLORMAP,
        crate::resources::ROOT_VISUAL,
    );
    create_window(
        &mut fixture.state,
        WINDOW_A,
        ROOT_WINDOW,
        Some(TEST_COLORMAP),
    );
    create_window(
        &mut fixture.state,
        WINDOW_B,
        ROOT_WINDOW,
        Some(TEST_COLORMAP),
    );
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let masks = &mut fixture
            .state
            .clients
            .get_mut(&client.0)
            .unwrap()
            .event_masks;
        masks.insert(WINDOW_A, COLORMAP_MASK);
        masks.insert(WINDOW_B, COLORMAP_MASK);
    }
    fixture
}

fn colormap_request(fixture: &mut NotifyFixture, opcode: u8, sequence: u16) {
    assert!(matches!(
        request(
            fixture,
            NOTIFY_SENDER,
            sequence,
            opcode,
            0,
            &TEST_COLORMAP.0.to_le_bytes()
        ),
        RequestOutcome::Handled
    ));
}

fn colormap_windows(bytes: &[u8]) -> Vec<ResourceId> {
    let mut windows: Vec<_> = wire_frames(bytes)
        .into_iter()
        .filter(|frame| frame[0] & 0x7f == 32)
        .map(|frame| ResourceId(u32::from_le_bytes(frame[4..8].try_into().unwrap())))
        .collect();
    windows.sort_unstable_by_key(|window| window.0);
    windows
}

fn literal_colormap_notify(window: ResourceId) -> Vec<u8> {
    let mut event = vec![32, 0, 0, 0];
    event.extend_from_slice(&window.0.to_le_bytes());
    event.extend_from_slice(&TEST_COLORMAP.0.to_le_bytes());
    event.extend_from_slice(&[0, 1]); // new=false, installed=true
    event.resize(32, 0);
    event
}

fn colormap_install_output(allow_policy: bool) -> (Vec<u8>, Vec<u8>) {
    let mut fixture = colormap_fixture();
    assert!(!fixture.state.policies_installed());
    if allow_policy {
        fixture.install_policy(|_, _| Decision::Allow);
    }
    colormap_request(&mut fixture, 81, 53);
    (
        fixture.drain(NOTIFY_RECEIVER_DENIED),
        fixture.drain(NOTIFY_RECEIVER_ALLOWED),
    )
}

// Mutation: suppress N38 ColormapNotify only when no access policy is installed.
#[test]
fn receive_colormap_no_policy_matches_independent_bytes() {
    let (without_policy_denied, without_policy_allowed) = colormap_install_output(false);
    let (allow_denied, allow_allowed) = colormap_install_output(true);
    let expected = [
        literal_colormap_notify(WINDOW_A),
        literal_colormap_notify(WINDOW_B),
    ];
    let sorted = |bytes: &[u8]| {
        let mut frames = wire_frames(bytes);
        frames.sort();
        frames
    };
    assert_eq!(sorted(&without_policy_denied), expected);
    assert_eq!(sorted(&without_policy_allowed), expected);
    assert_eq!(sorted(&allow_denied), expected);
    assert_eq!(sorted(&allow_allowed), expected);
    assert_eq!(sorted(&without_policy_denied), sorted(&allow_denied));
    assert_eq!(sorted(&without_policy_allowed), sorted(&allow_allowed));
}

// Mutations: use W(colormap) as the subject, or skip the installed-list update on denial.
#[test]
fn receive_colormap_uses_each_subscribing_window() {
    let mut fixture = colormap_fixture();
    let deny_receives = Arc::new(AtomicBool::new(false));
    let should_deny = Arc::clone(&deny_receives);
    let calls = fixture.install_policy(move |_, call| {
        if should_deny.load(Ordering::Relaxed)
            && call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(WINDOW_A))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });

    // Run Allow controls first on this resource table so its existing colormap-window
    // iteration order is unchanged when the denied deliveries follow.
    colormap_request(&mut fixture, 81, 51);
    let control_install_denied = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let control_install_allowed = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        colormap_windows(&control_install_allowed),
        [WINDOW_A, WINDOW_B]
    );
    let installed_control = fixture.state.installed_colormaps.clone();
    assert_eq!(installed_control, [TEST_COLORMAP]);
    colormap_request(&mut fixture, 82, 52);
    let control_uninstall_denied = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let control_uninstall_allowed = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        colormap_windows(&control_uninstall_allowed),
        [WINDOW_A, WINDOW_B]
    );
    let reinstalled_control = fixture.state.installed_colormaps.clone();
    assert_eq!(reinstalled_control, [ROOT_COLORMAP]);

    deny_receives.store(true, Ordering::Relaxed);
    colormap_request(&mut fixture, 81, 53);
    let denied_install = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let allowed_install = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(colormap_windows(&denied_install), [WINDOW_B]);
    assert_eq!(
        denied_install,
        filter_event_window(&control_install_denied, 32, WINDOW_A)
    );
    assert_eq!(allowed_install, control_install_allowed);
    assert_eq!(fixture.state.installed_colormaps, installed_control);

    colormap_request(&mut fixture, 82, 54);
    let denied_uninstall = fixture.drain(NOTIFY_RECEIVER_DENIED);
    let allowed_uninstall = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(colormap_windows(&denied_uninstall), [WINDOW_B]);
    assert_eq!(
        denied_uninstall,
        filter_event_window(&control_uninstall_denied, 32, WINDOW_A)
    );
    assert_eq!(allowed_uninstall, control_uninstall_allowed);
    assert_eq!(fixture.state.installed_colormaps, reinstalled_control);
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_DENIED.0)
    );
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_ALLOWED.0)
    );

    let recorded = calls.lock().unwrap();
    let receive = receives(&recorded);
    for opcode in [81, 82] {
        let rows: Vec<_> = receive
            .iter()
            .filter(|call| call.request == request_opcodes(opcode, 0))
            .collect();
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|call| {
            call.events == Some(vec![EventDescription::core(32)])
                && matches!(
                    call.receive,
                    Some(EventSubject::Window(WINDOW_A | WINDOW_B))
                )
        }));
    }
}

fn property_policy(fixture: &mut NotifyFixture) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == NOTIFY_RECEIVER_DENIED {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    })
}

fn seed_property(fixture: &mut NotifyFixture, property: AtomId, data: &[u8]) -> AtomId {
    let type_atom = fixture.state.atoms.intern("TASK6_NOTIFY_TYPE", false);
    fixture.state.resources.set_window_property(
        NOTIFY_WINDOW,
        property,
        PropertyValue {
            r#type: type_atom,
            format: PropertyFormat::F8,
            data: data.to_vec(),
        },
    );
    type_atom
}

fn property_event(property: AtomId, deleted: bool) -> Vec<u8> {
    let mut event = literal_property_notify(ClientByteOrder::LittleEndian, 0x5678, property);
    event[16] = u8::from(deleted);
    event
}

fn literal_get_property_reply(sequence: u16, type_atom: AtomId, value: &[u8]) -> Vec<u8> {
    assert_eq!(value.len(), 10, "fixture reply has ten property bytes");
    let mut reply = vec![1, 8];
    reply.extend_from_slice(&sequence.to_be_bytes());
    reply.extend_from_slice(&3_u32.to_be_bytes());
    reply.extend_from_slice(&type_atom.0.to_be_bytes());
    reply.extend_from_slice(&0_u32.to_be_bytes());
    reply.extend_from_slice(&10_u32.to_be_bytes());
    reply.extend_from_slice(&[0; 12]);
    reply.extend_from_slice(value);
    reply.extend_from_slice(&[0; 2]);
    reply
}

fn assert_property_calls(calls: &[RecordedExtensionCheck], request_opcode: u8, count: usize) {
    let receive = receives(calls);
    assert_eq!(receive.len(), count);
    for call in &receive {
        assert_eq!(call.receive, Some(EventSubject::Window(NOTIFY_WINDOW)));
        assert_eq!(call.events, Some(vec![EventDescription::core(28)]));
        assert_eq!(call.request, request_opcodes(request_opcode, 0));
    }
    assert!(!calls.iter().any(|call| call.hook == ExtensionHook::Send));
    let mut clients: Vec<_> = receive.iter().map(|call| call.client).collect();
    clients.sort_unstable_by_key(|client| client.0);
    if count == 2 {
        assert_eq!(clients, [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]);
    } else {
        assert_eq!(
            clients,
            [
                NOTIFY_RECEIVER_DENIED,
                NOTIFY_RECEIVER_DENIED,
                NOTIFY_RECEIVER_ALLOWED,
                NOTIFY_RECEIVER_ALLOWED,
            ]
        );
    }
}

// Mutations: drop mixed request output/reply, or skip deletion/rotation on denial.
#[test]
fn receive_get_property_delete_keeps_reply() {
    fn get_delete_output(allow_policy: bool) -> (Vec<u8>, Vec<u8>, AtomId, AtomId) {
        let mut fixture = NotifyFixture::new();
        select_property_change(&mut fixture);
        let property = fixture.state.atoms.intern("TASK6_GET_DELETE", false);
        let type_atom = seed_property(&mut fixture, property, b"reply-data");
        assert!(!fixture.state.policies_installed());
        if allow_policy {
            fixture.install_policy(|_, _| Decision::Allow);
        }
        let mut body = NOTIFY_WINDOW.0.to_le_bytes().to_vec();
        body.extend_from_slice(&property.0.to_le_bytes());
        body.extend_from_slice(&type_atom.0.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&1024_u32.to_le_bytes());
        assert!(matches!(
            request(&mut fixture, NOTIFY_RECEIVER_DENIED, 64, 20, 1, &body),
            RequestOutcome::Handled
        ));
        (
            fixture.drain(NOTIFY_RECEIVER_DENIED),
            fixture.drain(NOTIFY_RECEIVER_ALLOWED),
            property,
            type_atom,
        )
    }

    let (baseline_mixed, baseline_allowed, get_property, get_type) = get_delete_output(false);
    let (allow_mixed, allow_allowed, allow_property, allow_type) = get_delete_output(true);
    assert_eq!((allow_property, allow_type), (get_property, get_type));
    let mut expected_reply = literal_get_property_reply(64, get_type, b"reply-data");
    let mut expected_event = literal_property_notify(ClientByteOrder::BigEndian, 64, get_property);
    expected_event[16] = 1;
    expected_reply.extend_from_slice(&expected_event);
    assert_eq!(
        baseline_mixed, expected_reply,
        "no-policy reply and delete event"
    );
    assert_eq!(baseline_allowed, property_event(get_property, true));
    assert_eq!(allow_mixed, baseline_mixed, "recording-Allow mixed output");
    assert_eq!(
        allow_allowed, baseline_allowed,
        "recording-Allow fanout bytes"
    );

    let mut changed = NotifyFixture::new();
    select_property_change(&mut changed);
    let property = changed.state.atoms.intern("TASK6_CHANGE", false);
    let calls = property_policy(&mut changed);
    assert!(matches!(
        run_property_change(&mut changed, 61, property, b"changed"),
        RequestOutcome::Handled
    ));
    assert!(changed.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    let allowed_change = changed.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allowed_change, property_event(property, false));
    assert_eq!(
        changed
            .state
            .resources
            .window_property(NOTIFY_WINDOW, property)
            .unwrap()
            .data,
        b"changed"
    );
    assert_property_calls(&calls.lock().unwrap(), 18, 2);

    let mut deleted = NotifyFixture::new();
    select_property_change(&mut deleted);
    let property = deleted.state.atoms.intern("TASK6_DELETE", false);
    seed_property(&mut deleted, property, b"delete-me");
    let calls = property_policy(&mut deleted);
    assert!(matches!(
        request(
            &mut deleted,
            NOTIFY_RECEIVER_DENIED,
            62,
            19,
            0,
            &[NOTIFY_WINDOW.0.to_le_bytes(), property.0.to_le_bytes()].concat()
        ),
        RequestOutcome::Handled
    ));
    assert!(deleted.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    let allowed_delete = deleted.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allowed_delete, property_event(property, true));
    assert!(
        deleted
            .state
            .resources
            .window_property(NOTIFY_WINDOW, property)
            .is_none()
    );
    assert_property_calls(&calls.lock().unwrap(), 19, 2);

    let mut rotated = NotifyFixture::new();
    select_property_change(&mut rotated);
    let prop_a = rotated.state.atoms.intern("TASK6_ROTATE_A", false);
    let prop_b = rotated.state.atoms.intern("TASK6_ROTATE_B", false);
    let type_atom = rotated.state.atoms.intern("TASK6_ROTATE_TYPE", false);
    for (prop, byte) in [(prop_a, b'a'), (prop_b, b'b')] {
        rotated.state.resources.set_window_property(
            NOTIFY_WINDOW,
            prop,
            PropertyValue {
                r#type: type_atom,
                format: PropertyFormat::F8,
                data: vec![byte],
            },
        );
    }
    let calls = property_policy(&mut rotated);
    let mut rotate_body = NOTIFY_WINDOW.0.to_le_bytes().to_vec();
    rotate_body.extend_from_slice(&2_u16.to_le_bytes());
    rotate_body.extend_from_slice(&1_i16.to_le_bytes());
    rotate_body.extend_from_slice(&prop_a.0.to_le_bytes());
    rotate_body.extend_from_slice(&prop_b.0.to_le_bytes());
    assert!(matches!(
        request(
            &mut rotated,
            NOTIFY_RECEIVER_DENIED,
            63,
            114,
            0,
            &rotate_body
        ),
        RequestOutcome::Handled
    ));
    assert!(rotated.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    let allowed_rotate = rotated.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        allowed_rotate,
        [property_event(prop_a, false), property_event(prop_b, false)].concat()
    );
    let rotated_atoms: Vec<_> = wire_frames(&allowed_rotate)
        .iter()
        .map(|frame| AtomId(u32::from_le_bytes(frame[8..12].try_into().unwrap())))
        .collect();
    assert_eq!(rotated_atoms, [prop_a, prop_b]);
    assert_eq!(
        rotated
            .state
            .resources
            .window_property(NOTIFY_WINDOW, prop_a)
            .unwrap()
            .data,
        b"b"
    );
    assert_eq!(
        rotated
            .state
            .resources
            .window_property(NOTIFY_WINDOW, prop_b)
            .unwrap()
            .data,
        b"a"
    );
    assert_property_calls(&calls.lock().unwrap(), 114, 4);

    let mut get = NotifyFixture::new();
    select_property_change(&mut get);
    let property = get.state.atoms.intern("TASK6_GET_DELETE", false);
    let type_atom = seed_property(&mut get, property, b"reply-data");
    let calls = property_policy(&mut get);
    let mut get_body = NOTIFY_WINDOW.0.to_le_bytes().to_vec();
    get_body.extend_from_slice(&property.0.to_le_bytes());
    get_body.extend_from_slice(&type_atom.0.to_le_bytes());
    get_body.extend_from_slice(&0_u32.to_le_bytes());
    get_body.extend_from_slice(&1024_u32.to_le_bytes());
    assert!(matches!(
        request(&mut get, NOTIFY_RECEIVER_DENIED, 64, 20, 1, &get_body),
        RequestOutcome::Handled
    ));
    let mixed = get.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        mixed.len(),
        44,
        "full reply without a denied PropertyNotify"
    );
    assert_eq!(
        mixed[0], 1,
        "full GetProperty reply precedes the denied event"
    );
    assert_eq!(mixed[1], 8, "reply format remains intact");
    assert_eq!(u16::from_be_bytes(mixed[2..4].try_into().unwrap()), 64);
    assert_eq!(
        u32::from_be_bytes(mixed[8..12].try_into().unwrap()),
        type_atom.0
    );
    assert_eq!(u32::from_be_bytes(mixed[12..16].try_into().unwrap()), 0);
    assert_eq!(u32::from_be_bytes(mixed[16..20].try_into().unwrap()), 10);
    assert_eq!(&mixed[32..42], b"reply-data");
    let allowed_get_delete = get.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allowed_get_delete, property_event(property, true));
    assert!(
        get.state
            .resources
            .window_property(NOTIFY_WINDOW, property)
            .is_none()
    );
    assert_property_calls(&calls.lock().unwrap(), 20, 2);
    for fixture in [&changed, &deleted, &rotated, &get] {
        assert!(
            fixture
                .state
                .clients
                .contains_key(&NOTIFY_RECEIVER_DENIED.0)
        );
        assert!(
            fixture
                .state
                .clients
                .contains_key(&NOTIFY_RECEIVER_ALLOWED.0)
        );
    }
}

fn rootless_property_fixture() -> (NotifyFixture, AtomId) {
    let mut fixture = NotifyFixture::new();
    fixture.backend = RecordingBackend::new().with_redirect_activation();
    rootless::enable(
        &mut fixture.state,
        &mut fixture.backend,
        CoordinatePolicy::Opaque,
    )
    .expect("enable rootless mode");
    let _ = rootless::selection::selection_window(&mut fixture.state);
    create_window(&mut fixture.state, NOTIFY_WINDOW, ROOT_WINDOW, None);
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .unwrap()
            .event_masks
            .insert(NOTIFY_WINDOW, PROPERTY_CHANGE_MASK);
    }
    let denied = fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .unwrap();
    denied.byte_order = ClientByteOrder::BigEndian;
    denied.last_sequence.store(0x1234, Ordering::Relaxed);
    let allowed = fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .unwrap();
    allowed.byte_order = ClientByteOrder::LittleEndian;
    allowed.last_sequence.store(0x5678, Ordering::Relaxed);
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
    rootless::selection::watch_properties(&mut fixture.state, NOTIFY_WINDOW, true);
    let property = fixture.state.atoms.intern("TASK6_ROOTLESS_PROPERTY", false);
    (fixture, property)
}

fn rootless_change(fixture: &mut NotifyFixture, window: ResourceId, property: AtomId, seq: u16) {
    let body = property_change_body(window, property, b"bridge-data");
    assert!(matches!(
        request(fixture, NOTIFY_SENDER, seq, 18, 0, &body),
        RequestOutcome::Handled
    ));
}

// Mutations: check after push, deny the entire emitter, or ask the bridge on an unwatched window.
#[test]
fn receive_bridge_property_denial_keeps_clients_and_property() {
    let (mut control, control_property) = rootless_property_fixture();
    assert!(!control.state.policies_installed());
    rootless_change(&mut control, NOTIFY_WINDOW, control_property, 71);
    let control_denied_client = control.drain(NOTIFY_RECEIVER_DENIED);
    let control_allowed_client = control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(control_denied_client.len(), 32);
    assert_eq!(control_allowed_client.len(), 32);
    assert_eq!(
        control_denied_client,
        literal_property_notify(ClientByteOrder::BigEndian, 0x1234, control_property),
        "no-policy bridge PropertyNotify for the big-endian client"
    );
    assert_eq!(
        control_allowed_client,
        literal_property_notify(ClientByteOrder::LittleEndian, 0x5678, control_property),
        "no-policy bridge PropertyNotify for the little-endian client"
    );
    assert!(matches!(
        rootless::selection::take_selection_events(&mut control.state).as_slice(),
        [SelectionEvent::Property { window, property, .. }]
            if *window == NOTIFY_WINDOW && *property == control_property
    ));

    let (mut allow, allow_property) = rootless_property_fixture();
    assert_eq!(allow_property, control_property);
    allow.install_policy(|_, _| Decision::Allow);
    rootless_change(&mut allow, NOTIFY_WINDOW, allow_property, 71);
    assert_eq!(allow.drain(NOTIFY_RECEIVER_DENIED), control_denied_client);
    assert_eq!(allow.drain(NOTIFY_RECEIVER_ALLOWED), control_allowed_client);

    let (mut fixture, property) = rootless_property_fixture();
    assert_eq!(property, control_property);
    let deny_bridge = Arc::new(AtomicBool::new(true));
    let should_deny_bridge = Arc::clone(&deny_bridge);
    let calls = fixture.install_policy(move |_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == SERVER_OWNER
            && should_deny_bridge.load(Ordering::Relaxed)
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    rootless_change(&mut fixture, NOTIFY_WINDOW, property, 71);
    let denied_client = fixture.drain(NOTIFY_RECEIVER_DENIED);
    assert_eq!(denied_client, control_denied_client);
    let allowed = fixture.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allowed, control_allowed_client);
    assert_eq!(allowed[0], 28);
    assert!(rootless::selection::take_selection_events(&mut fixture.state).is_empty());
    assert_eq!(
        fixture
            .state
            .resources
            .window_property(NOTIFY_WINDOW, property)
            .unwrap()
            .data,
        b"bridge-data"
    );
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_DENIED.0)
    );
    assert!(
        fixture
            .state
            .clients
            .contains_key(&NOTIFY_RECEIVER_ALLOWED.0)
    );
    let recorded = calls.lock().unwrap();
    let bridge_check = receives(&recorded)
        .into_iter()
        .find(|call| call.client == SERVER_OWNER)
        .expect("checked bridge receiver");
    assert_eq!(
        bridge_check.receive,
        Some(EventSubject::Window(NOTIFY_WINDOW))
    );
    assert_eq!(bridge_check.events, Some(vec![EventDescription::core(28)]));
    assert_eq!(bridge_check.request, request_opcodes(18, 0));
    assert!(!recorded.iter().any(|call| call.hook == ExtensionHook::Send));
    drop(recorded);

    let unwatched = ResourceId(0x0055_0060);
    create_window(&mut fixture.state, unwatched, ROOT_WINDOW, None);
    rootless_change(&mut fixture, unwatched, property, 72);
    assert!(
        !receives(&calls.lock().unwrap())
            .iter()
            .any(|call| call.client == SERVER_OWNER
                && call.receive == Some(EventSubject::Window(unwatched)))
    );

    // The watched window remains watched after the denial; the next Allow delivery reaches it.
    deny_bridge.store(false, Ordering::Relaxed);
    rootless_change(&mut fixture, NOTIFY_WINDOW, property, 73);
    assert!(matches!(
        rootless::selection::take_selection_events(&mut fixture.state).as_slice(),
        [SelectionEvent::Property { window, property: event_property, deleted: false, .. }]
            if *window == NOTIFY_WINDOW && *event_property == property
    ));

    // The bridge-owned window uses the same gate for independent server work (request=None).
    deny_bridge.store(true, Ordering::Relaxed);
    let bridge_window = rootless::selection::selection_window(&mut fixture.state);
    crate::core_loop::process_request::props::change_property_as_server(
        &mut fixture.state,
        &mut fixture.backend,
        bridge_window,
        property,
        AtomId(31),
        PropertyFormat::F8,
        b"server-data".to_vec(),
    );
    assert!(rootless::selection::take_selection_events(&mut fixture.state).is_empty());
    assert!(fixture.state.receive_request.is_none());
    let all_calls = calls.lock().unwrap();
    assert!(receives(&all_calls).iter().any(|call| {
        call.client == SERVER_OWNER
            && call.receive == Some(EventSubject::Window(bridge_window))
            && call.events == Some(vec![EventDescription::core(28)])
            && call.request.is_none()
    }));
    assert!(
        !all_calls
            .iter()
            .any(|call| call.hook == ExtensionHook::Send)
    );
}
