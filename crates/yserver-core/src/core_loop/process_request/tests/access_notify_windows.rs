use super::{
    access_notify_support::*,
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject},
    backend::{WindowHandle, recording::RecordedCall},
    core_loop::fanout::expose_event_fanout_to_state,
    host_x11::{HostExposeEvent, HostXidMap},
    resources::{MapState, ROOT_WINDOW},
};

const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};
const EXPOSURE_MASK: u32 = 0x0000_8000;
const VISIBILITY_MASK: u32 = 0x0001_0000;
const PARENT_WINDOW: ResourceId = ResourceId(0x0051_0010);
const CHILD_WINDOW: ResourceId = ResourceId(0x0051_0011);
const GRANDCHILD_WINDOW: ResourceId = ResourceId(0x0051_0012);
const HOST_WINDOW: u32 = 0x0052_0010;

fn add_window(
    fixture: &mut NotifyFixture,
    id: ResourceId,
    parent: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    background_pixel: Option<u32>,
) {
    fixture.state.resources.create_window(
        NOTIFY_SENDER,
        x11::CreateWindowRequest {
            depth: 24,
            window: id,
            parent,
            x,
            y,
            width,
            height,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            background_pixel,
            ..Default::default()
        },
    );
}

fn select_mask(fixture: &mut NotifyFixture, window: ResourceId, mask: u32) {
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("receiver client")
            .event_masks
            .insert(window, mask);
    }
}

fn receives(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

fn literal_expose(
    window: ResourceId,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    count: u16,
) -> Vec<u8> {
    let mut bytes = vec![12, 0, 0, 0];
    bytes.extend_from_slice(&window.0.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.resize(32, 0);
    bytes
}

fn expose_fields(bytes: &[u8]) -> Vec<(ResourceId, u16, u16, u16, u16, u16)> {
    bytes
        .chunks_exact(32)
        .map(|event| {
            assert_eq!(event[0], 12, "every frame must be Expose");
            (
                ResourceId(u32::from_le_bytes(event[4..8].try_into().unwrap())),
                u16::from_le_bytes(event[8..10].try_into().unwrap()),
                u16::from_le_bytes(event[10..12].try_into().unwrap()),
                u16::from_le_bytes(event[12..14].try_into().unwrap()),
                u16::from_le_bytes(event[14..16].try_into().unwrap()),
                u16::from_le_bytes(event[16..18].try_into().unwrap()),
            )
        })
        .collect()
}

fn visibility_fields(bytes: &[u8]) -> Vec<(ResourceId, u8)> {
    bytes
        .chunks_exact(32)
        .map(|event| {
            assert_eq!(event[0], 15, "every frame must be VisibilityNotify");
            (
                ResourceId(u32::from_le_bytes(event[4..8].try_into().unwrap())),
                event[8],
            )
        })
        .collect()
}

fn clear_area(fixture: &mut NotifyFixture, sequence: u16, exposures: bool) -> RequestOutcome {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&NOTIFY_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes());
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 61,
                data: u8::from(exposures),
                length_units: 4,
            },
            &body,
        )
        .expect("ClearArea through production dispatcher")
}

fn configured_window_fixture(window: ResourceId, width: u16, height: u16) -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    add_window(&mut fixture, window, ROOT_WINDOW, 0, 0, width, height, None);
    fixture
        .state
        .resources
        .window_mut(window)
        .expect("configured window")
        .map_state = MapState::Viewable;
    select_mask(&mut fixture, window, EXPOSURE_MASK);
    fixture
}

fn configure_width(fixture: &mut NotifyFixture, window: ResourceId, sequence: u16, width: u32) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&0x0004_u16.to_le_bytes()); // CWWidth
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&width.to_le_bytes());
    assert!(matches!(
        fixture
            .request(
                NOTIFY_SENDER,
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 12,
                    data: 0,
                    length_units: 4,
                },
                &body,
            )
            .expect("ConfigureWindow through production dispatcher"),
        RequestOutcome::Handled
    ));
}

// Mutations: use the ancestor subject for descendant Expose; return on the first denial;
// and ask policy for an unknown host XID or an unsubscribed target.
#[test]
fn receive_host_expose_checks_each_delivery_window() {
    fn fixture() -> NotifyFixture {
        let mut fixture = NotifyFixture::new();
        add_window(
            &mut fixture,
            PARENT_WINDOW,
            ROOT_WINDOW,
            0,
            0,
            100,
            100,
            None,
        );
        add_window(
            &mut fixture,
            CHILD_WINDOW,
            PARENT_WINDOW,
            10,
            20,
            30,
            40,
            None,
        );
        for window in [PARENT_WINDOW, CHILD_WINDOW] {
            fixture
                .state
                .resources
                .window_mut(window)
                .expect("mapped expose window")
                .map_state = MapState::Viewable;
            select_mask(&mut fixture, window, EXPOSURE_MASK);
        }
        fixture
    }

    let event = HostExposeEvent {
        host_xid: HOST_WINDOW,
        x: 0,
        y: 0,
        width: 100,
        height: 100,
        count: 1,
    };
    let map = HostXidMap::from([(HOST_WINDOW, PARENT_WINDOW)]);
    let mut allow = fixture();
    allow.install_policy(|_, _| Decision::Allow);
    assert!(
        expose_event_fanout_to_state(&mut allow.state, &map, event).is_empty(),
        "allow-control host delivery must not disconnect clients"
    );
    let allow_denied = allow.drain(NOTIFY_RECEIVER_DENIED);
    let allow_permitted = allow.drain(NOTIFY_RECEIVER_ALLOWED);
    let expected_parent = literal_expose(PARENT_WINDOW, 0, 0, 100, 100, 1);
    let expected_child = literal_expose(CHILD_WINDOW, 0, 0, 30, 40, 0);
    assert_eq!(
        allow_denied,
        [expected_parent.clone(), expected_child.clone()].concat()
    );
    assert_eq!(allow_permitted, allow_denied);

    let mut denied = fixture();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(PARENT_WINDOW))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(
        expose_event_fanout_to_state(&mut denied.state, &map, event).is_empty(),
        "Receive denial must not be reported as a transport failure"
    );
    assert_eq!(
        receives(&calls.lock().unwrap()).len(),
        4,
        "the policy must be asked once for each selected window/client delivery"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(denied.drain(NOTIFY_RECEIVER_DENIED), expected_child);
    let permitted = denied.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(permitted, allow_permitted);
    assert_eq!(
        expose_fields(&permitted),
        vec![
            (PARENT_WINDOW, 0, 0, 100, 100, 1),
            (CHILD_WINDOW, 0, 0, 30, 40, 0),
        ]
    );
    for window in [PARENT_WINDOW, CHILD_WINDOW] {
        assert_eq!(
            denied.state.resources.window(window).unwrap().map_state,
            allow.state.resources.window(window).unwrap().map_state
        );
    }
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        assert!(denied.state.clients.contains_key(&client.0));
    }
    assert_eq!(denied.backend.calls(), allow.backend.calls());

    let no_selectors = ResourceId(0x0051_0013);
    add_window(&mut denied, no_selectors, ROOT_WINDOW, 0, 0, 20, 20, None);
    let before = receives(&calls.lock().unwrap()).len();
    assert!(
        expose_event_fanout_to_state(
            &mut denied.state,
            &map,
            HostExposeEvent {
                host_xid: HOST_WINDOW + 1,
                ..event
            },
        )
        .is_empty()
    );
    let empty_map = HostXidMap::from([(HOST_WINDOW + 2, no_selectors)]);
    assert!(
        expose_event_fanout_to_state(
            &mut denied.state,
            &empty_map,
            HostExposeEvent {
                host_xid: HOST_WINDOW + 2,
                ..event
            },
        )
        .is_empty()
    );
    assert_eq!(receives(&calls.lock().unwrap()).len(), before);

    let checks = calls.lock().unwrap();
    let receives = receives(&checks);
    assert_eq!(receives.len(), 4);
    let mut deliveries: Vec<_> = receives
        .iter()
        .map(|call| (call.client.0, call.receive.unwrap()))
        .collect();
    deliveries.sort_by_key(|(client, subject)| {
        let window = match subject {
            EventSubject::Window(window) => window.0,
            _ => 0,
        };
        (window, *client)
    });
    assert_eq!(
        deliveries,
        vec![
            (
                NOTIFY_RECEIVER_DENIED.0,
                EventSubject::Window(PARENT_WINDOW)
            ),
            (
                NOTIFY_RECEIVER_ALLOWED.0,
                EventSubject::Window(PARENT_WINDOW)
            ),
            (NOTIFY_RECEIVER_DENIED.0, EventSubject::Window(CHILD_WINDOW)),
            (
                NOTIFY_RECEIVER_ALLOWED.0,
                EventSubject::Window(CHILD_WINDOW)
            ),
        ]
    );
    for call in receives {
        assert_eq!(call.events, Some(vec![EventDescription::core(12)]));
        assert_eq!(call.request, None);
    }
}

// Mutation: skip ClearArea painting and backend work when any Expose recipient is denied.
#[test]
fn receive_clear_area_expose_denial_keeps_paint() {
    fn fixture() -> NotifyFixture {
        let mut fixture = NotifyFixture::new();
        add_window(
            &mut fixture,
            NOTIFY_WINDOW,
            ROOT_WINDOW,
            0,
            0,
            64,
            48,
            Some(0x0012_3456),
        );
        let window = fixture
            .state
            .resources
            .window_mut(NOTIFY_WINDOW)
            .expect("clearable window");
        window.map_state = MapState::Viewable;
        window.host_xid = Some(WindowHandle::from_raw_for_test(0x0053_0001));
        select_mask(&mut fixture, NOTIFY_WINDOW, EXPOSURE_MASK);
        fixture
    }

    let mut allow = fixture();
    allow.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        clear_area(&mut allow, 31, true),
        RequestOutcome::Handled
    ));
    let allow_denied = allow.drain(NOTIFY_RECEIVER_DENIED);
    let allow_permitted = allow.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allow_denied, literal_expose(NOTIFY_WINDOW, 0, 0, 64, 48, 0));
    assert_eq!(allow_permitted, allow_denied);
    let allow_calls = allow.backend.calls();
    assert!(allow_calls.iter().any(|call| matches!(
        call,
        RecordedCall::FillRectangle {
            host_xid: 0x0053_0001,
            foreground: 0x0012_3456,
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        }
    )));

    let mut denied = fixture();
    let checks = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == NOTIFY_RECEIVER_DENIED {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(
        clear_area(&mut denied, 31, true),
        RequestOutcome::Handled
    ));
    assert_eq!(
        receives(&checks.lock().unwrap()).len(),
        2,
        "the policy must be asked once for each selected Expose recipient"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert!(denied.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(denied.drain(NOTIFY_RECEIVER_ALLOWED), allow_permitted);
    assert_eq!(denied.backend.calls(), allow_calls);
    assert_eq!(
        denied
            .state
            .resources
            .window(NOTIFY_WINDOW)
            .unwrap()
            .map_state,
        allow
            .state
            .resources
            .window(NOTIFY_WINDOW)
            .unwrap()
            .map_state
    );
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        assert!(denied.state.clients.contains_key(&client.0));
    }
    assert_eq!(
        expose_fields(&allow_permitted),
        vec![(NOTIFY_WINDOW, 0, 0, 64, 48, 0)]
    );
    let receives_before_no_exposure = receives(&checks.lock().unwrap()).len();

    let allow_calls_before_no_exposure = allow.backend.calls();
    assert!(matches!(
        clear_area(&mut allow, 32, false),
        RequestOutcome::Handled
    ));
    let denied_calls_before_no_exposure = denied.backend.calls();
    assert!(matches!(
        clear_area(&mut denied, 32, false),
        RequestOutcome::Handled
    ));
    assert!(allow.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert!(allow.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert!(denied.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert!(denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
    assert_eq!(
        allow.backend.calls().len(),
        allow_calls_before_no_exposure.len() + 1
    );
    assert_eq!(denied.backend.calls(), allow.backend.calls());
    assert_eq!(
        denied.backend.calls().len(),
        denied_calls_before_no_exposure.len() + 1
    );
    assert_eq!(
        receives(&checks.lock().unwrap()).len(),
        receives_before_no_exposure
    );

    let call_log = checks.lock().unwrap();
    let receives = receives(&call_log);
    assert_eq!(receives.len(), 2);
    for call in receives {
        assert_eq!(call.receive, Some(EventSubject::Window(NOTIFY_WINDOW)));
        assert_eq!(call.events, Some(vec![EventDescription::core(12)]));
        assert_eq!(call.request, request_opcodes(61, 0));
    }
}

// Mutations: describe Visibility as Expose, or prune the descendant walk after a root denial.
#[test]
fn receive_map_visibility_denial_keeps_descendant_walk() {
    fn fixture() -> NotifyFixture {
        let mut fixture = NotifyFixture::new();
        add_window(
            &mut fixture,
            PARENT_WINDOW,
            ROOT_WINDOW,
            0,
            0,
            100,
            100,
            None,
        );
        add_window(
            &mut fixture,
            CHILD_WINDOW,
            PARENT_WINDOW,
            5,
            5,
            60,
            60,
            None,
        );
        add_window(
            &mut fixture,
            GRANDCHILD_WINDOW,
            CHILD_WINDOW,
            5,
            5,
            20,
            20,
            None,
        );
        for window in [CHILD_WINDOW, GRANDCHILD_WINDOW] {
            fixture
                .state
                .resources
                .window_mut(window)
                .expect("pre-mapped descendant")
                .map_state = MapState::Unviewable;
        }
        for window in [PARENT_WINDOW, CHILD_WINDOW, GRANDCHILD_WINDOW] {
            select_mask(&mut fixture, window, VISIBILITY_MASK);
        }
        fixture
    }

    fn map_parent(fixture: &mut NotifyFixture) -> RequestOutcome {
        fixture
            .request(
                NOTIFY_SENDER,
                SequenceNumber(41),
                RequestHeader {
                    opcode: 8,
                    data: 0,
                    length_units: 2,
                },
                &PARENT_WINDOW.0.to_le_bytes(),
            )
            .expect("MapWindow through production dispatcher")
    }

    fn map_states(fixture: &NotifyFixture) -> Vec<(ResourceId, MapState)> {
        [PARENT_WINDOW, CHILD_WINDOW, GRANDCHILD_WINDOW]
            .into_iter()
            .map(|window| {
                (
                    window,
                    fixture.state.resources.window(window).unwrap().map_state,
                )
            })
            .collect()
    }

    let mut allow = fixture();
    allow.install_policy(|_, _| Decision::Allow);
    assert!(matches!(map_parent(&mut allow), RequestOutcome::Handled));
    let allow_denied = allow.drain(NOTIFY_RECEIVER_DENIED);
    let allow_permitted = allow.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allow_denied.len(), 3 * 32);
    assert_eq!(allow_permitted, allow_denied);

    let mut denied = fixture();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(PARENT_WINDOW))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    let before = map_states(&denied);
    assert_eq!(before[1].1, MapState::Unviewable);
    assert_eq!(before[2].1, MapState::Unviewable);
    assert!(matches!(map_parent(&mut denied), RequestOutcome::Handled));
    assert_eq!(
        receives(&calls.lock().unwrap()).len(),
        6,
        "root and both descendants must each check both selected recipients"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(denied.drain(NOTIFY_RECEIVER_DENIED), allow_denied[32..]);
    assert_eq!(denied.drain(NOTIFY_RECEIVER_ALLOWED), allow_permitted);
    assert_eq!(
        visibility_fields(&allow_permitted),
        vec![
            (PARENT_WINDOW, 0),
            (CHILD_WINDOW, 0),
            (GRANDCHILD_WINDOW, 0),
        ]
    );
    assert_eq!(
        visibility_fields(&allow_denied[32..]),
        vec![(CHILD_WINDOW, 0), (GRANDCHILD_WINDOW, 0)]
    );
    assert_eq!(map_states(&denied), map_states(&allow));
    assert_eq!(denied.backend.calls(), allow.backend.calls());
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));
    assert!(denied.state.receive_request.is_none());

    let calls = calls.lock().unwrap();
    let receives = receives(&calls);
    assert_eq!(receives.len(), 6);
    let mut deliveries: Vec<_> = receives
        .iter()
        .map(|call| (call.client.0, call.receive.unwrap()))
        .collect();
    deliveries.sort_by_key(|(client, subject)| {
        let window = match subject {
            EventSubject::Window(window) => window.0,
            _ => 0,
        };
        (window, *client)
    });
    let expected = [PARENT_WINDOW, CHILD_WINDOW, GRANDCHILD_WINDOW]
        .into_iter()
        .flat_map(|window| {
            [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED]
                .into_iter()
                .map(move |client| (client.0, EventSubject::Window(window)))
        })
        .collect::<Vec<_>>();
    assert_eq!(deliveries, expected);
    for call in receives {
        assert_eq!(call.events, Some(vec![EventDescription::core(15)]));
        assert_eq!(call.request, request_opcodes(8, 0));
    }
}

// Mutation: drop all remaining Expose rectangles after the first denied recipient.
#[test]
fn receive_expose_rectangles_keep_counts_and_collapse() {
    let split_window = ResourceId(0x0054_0001);
    let mut allow_split = configured_window_fixture(split_window, 100, 100);
    let mut denied_split = configured_window_fixture(split_window, 100, 100);
    let split_rects = vec![
        x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        x11::xfixes::RegionRect {
            x: 80,
            y: 70,
            width: 10,
            height: 20,
        },
    ];
    for fixture in [&mut allow_split, &mut denied_split] {
        assert!(crate::nested::set_shape_rects(
            &mut fixture.state,
            split_window,
            x11::shape::KIND_BOUNDING,
            split_rects.clone(),
        ));
    }
    allow_split.install_policy(|_, _| Decision::Allow);
    let denied_split_calls = denied_split.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == NOTIFY_RECEIVER_DENIED {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    configure_width(&mut allow_split, split_window, 51, 101);
    configure_width(&mut denied_split, split_window, 51, 101);
    assert_eq!(
        receives(&denied_split_calls.lock().unwrap()).len(),
        4,
        "each of the two Expose rectangles must check both recipients"
    );
    let allow_split_denied = allow_split.drain(NOTIFY_RECEIVER_DENIED);
    let allow_split_permitted = allow_split.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allow_split_denied, allow_split_permitted);
    assert_eq!(
        expose_fields(&allow_split_permitted),
        vec![
            (split_window, 0, 0, 10, 10, 1),
            (split_window, 80, 70, 10, 20, 0),
        ]
    );
    assert!(denied_split.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(
        denied_split.drain(NOTIFY_RECEIVER_ALLOWED),
        allow_split_permitted
    );
    assert_eq!(denied_split.backend.calls(), allow_split.backend.calls());
    assert!(denied_split.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(
        denied_split
            .state
            .resources
            .window(split_window)
            .map(|window| (window.width, window.height, window.map_state)),
        allow_split
            .state
            .resources
            .window(split_window)
            .map(|window| (window.width, window.height, window.map_state))
    );
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        assert!(denied_split.state.clients.contains_key(&client.0));
    }
    let split_call_log = denied_split_calls.lock().unwrap();
    let split_checks = receives(&split_call_log);
    assert_eq!(split_checks.len(), 4);
    for call in split_checks {
        assert_eq!(call.receive, Some(EventSubject::Window(split_window)));
        assert_eq!(call.events, Some(vec![EventDescription::core(12)]));
        assert_eq!(call.request, request_opcodes(12, 0));
    }

    let collapsed_window = ResourceId(0x0054_0002);
    let mut allow_collapsed = configured_window_fixture(collapsed_window, 300, 100);
    let mut denied_collapsed = configured_window_fixture(collapsed_window, 300, 100);
    let many_rects: Vec<_> = (0..26)
        .map(|index| x11::xfixes::RegionRect {
            x: i16::try_from(index * 10).unwrap(),
            y: 0,
            width: 1,
            height: 1,
        })
        .collect();
    for fixture in [&mut allow_collapsed, &mut denied_collapsed] {
        assert!(crate::nested::set_shape_rects(
            &mut fixture.state,
            collapsed_window,
            x11::shape::KIND_BOUNDING,
            many_rects.clone(),
        ));
    }
    allow_collapsed.install_policy(|_, _| Decision::Allow);
    let collapsed_calls = denied_collapsed.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == NOTIFY_RECEIVER_DENIED {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    configure_width(&mut allow_collapsed, collapsed_window, 52, 301);
    configure_width(&mut denied_collapsed, collapsed_window, 52, 301);
    assert_eq!(
        receives(&collapsed_calls.lock().unwrap()).len(),
        2,
        "the collapsed Expose must check both recipients once"
    );
    let allow_collapsed_denied = allow_collapsed.drain(NOTIFY_RECEIVER_DENIED);
    let allow_collapsed_permitted = allow_collapsed.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(allow_collapsed_denied, allow_collapsed_permitted);
    assert_eq!(
        expose_fields(&allow_collapsed_permitted),
        vec![(collapsed_window, 0, 0, 251, 1, 0)]
    );
    assert!(denied_collapsed.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(
        denied_collapsed.drain(NOTIFY_RECEIVER_ALLOWED),
        allow_collapsed_permitted
    );
    assert_eq!(
        denied_collapsed.backend.calls(),
        allow_collapsed.backend.calls()
    );
    assert!(denied_collapsed.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(
        denied_collapsed
            .state
            .resources
            .window(collapsed_window)
            .map(|window| (window.width, window.height, window.map_state)),
        allow_collapsed
            .state
            .resources
            .window(collapsed_window)
            .map(|window| (window.width, window.height, window.map_state))
    );
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        assert!(denied_collapsed.state.clients.contains_key(&client.0));
    }
    let collapsed_call_log = collapsed_calls.lock().unwrap();
    let collapsed_checks = receives(&collapsed_call_log);
    assert_eq!(collapsed_checks.len(), 2);
    for call in collapsed_checks {
        assert_eq!(call.receive, Some(EventSubject::Window(collapsed_window)));
        assert_eq!(call.events, Some(vec![EventDescription::core(12)]));
        assert_eq!(call.request, request_opcodes(12, 0));
    }
}
