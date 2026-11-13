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
const NEW_PARENT_WINDOW: ResourceId = ResourceId(0x0051_0013);
const CIRCULATE_SIBLING: ResourceId = ResourceId(0x0051_0014);
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

#[derive(Clone, Copy)]
struct StructureDelivery {
    id: &'static str,
    event_type: u8,
    delivery_window: ResourceId,
    affected_window: ResourceId,
    reparent: Option<ResourceId>,
}

#[derive(Debug, PartialEq, Eq)]
struct StructureEvent {
    event_type: u8,
    sequence: u16,
    delivery_window: ResourceId,
    affected_window: ResourceId,
    reparent: Option<ResourceId>,
}

#[derive(Clone, Copy)]
enum StructureRequest {
    Create,
    Map,
    Unmap,
    Configure,
    Reparent,
    MapSubwindows,
    UnmapSubwindows,
    Circulate,
}

struct StructureCase {
    name: &'static str,
    request: StructureRequest,
    opcode: u8,
    deny_window: ResourceId,
    deliveries: &'static [StructureDelivery],
}

fn structure_fixture(request: StructureRequest) -> NotifyFixture {
    const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
    const SUBSTRUCTURE_NOTIFY: u32 = 0x0008_0000;

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
        NEW_PARENT_WINDOW,
        ROOT_WINDOW,
        150,
        0,
        100,
        100,
        None,
    );
    fixture
        .state
        .resources
        .window_mut(PARENT_WINDOW)
        .expect("old parent")
        .map_state = MapState::Viewable;
    fixture
        .state
        .resources
        .window_mut(NEW_PARENT_WINDOW)
        .expect("new parent")
        .map_state = MapState::Viewable;

    if !matches!(request, StructureRequest::Create) {
        add_window(
            &mut fixture,
            CHILD_WINDOW,
            PARENT_WINDOW,
            5,
            6,
            10,
            11,
            None,
        );
        let map_state = if matches!(
            request,
            StructureRequest::Map | StructureRequest::MapSubwindows
        ) {
            MapState::Unmapped
        } else {
            MapState::Viewable
        };
        fixture
            .state
            .resources
            .window_mut(CHILD_WINDOW)
            .expect("child")
            .map_state = map_state;
    }
    if matches!(request, StructureRequest::Circulate) {
        add_window(
            &mut fixture,
            CIRCULATE_SIBLING,
            PARENT_WINDOW,
            5,
            6,
            10,
            11,
            None,
        );
        fixture
            .state
            .resources
            .window_mut(CIRCULATE_SIBLING)
            .expect("overlapping sibling")
            .map_state = MapState::Viewable;
    }

    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let masks = &mut fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("receiver")
            .event_masks;
        masks.insert(PARENT_WINDOW, SUBSTRUCTURE_NOTIFY);
        masks.insert(NEW_PARENT_WINDOW, SUBSTRUCTURE_NOTIFY);
        masks.insert(CHILD_WINDOW, STRUCTURE_NOTIFY);
    }
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .expect("denied receiver")
        .byte_order = ClientByteOrder::BigEndian;
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .expect("denied receiver")
        .last_sequence
        .store(0x1234, std::sync::atomic::Ordering::Relaxed);
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("allowed receiver")
        .last_sequence
        .store(0x5678, std::sync::atomic::Ordering::Relaxed);
    fixture
}

fn structure_request(fixture: &mut NotifyFixture, request: StructureRequest) -> RequestOutcome {
    let (header, body) = match request {
        StructureRequest::Create => {
            let mut body = Vec::with_capacity(28);
            body.extend_from_slice(&CHILD_WINDOW.0.to_le_bytes());
            body.extend_from_slice(&PARENT_WINDOW.0.to_le_bytes());
            body.extend_from_slice(&5_i16.to_le_bytes());
            body.extend_from_slice(&6_i16.to_le_bytes());
            body.extend_from_slice(&10_u16.to_le_bytes());
            body.extend_from_slice(&11_u16.to_le_bytes());
            body.extend_from_slice(&0_u16.to_le_bytes()); // border width
            body.extend_from_slice(&1_u16.to_le_bytes()); // InputOutput
            body.extend_from_slice(&crate::resources::ROOT_VISUAL.0.to_le_bytes());
            body.extend_from_slice(&0_u32.to_le_bytes()); // value mask
            (
                RequestHeader {
                    opcode: 1,
                    data: 24,
                    length_units: 8,
                },
                body,
            )
        }
        StructureRequest::Map => (
            RequestHeader {
                opcode: 8,
                data: 0,
                length_units: 2,
            },
            CHILD_WINDOW.0.to_le_bytes().to_vec(),
        ),
        StructureRequest::Unmap => (
            RequestHeader {
                opcode: 10,
                data: 0,
                length_units: 2,
            },
            CHILD_WINDOW.0.to_le_bytes().to_vec(),
        ),
        StructureRequest::Configure => {
            let mut body = Vec::with_capacity(12);
            body.extend_from_slice(&CHILD_WINDOW.0.to_le_bytes());
            body.extend_from_slice(&1_u16.to_le_bytes()); // CWX
            body.extend_from_slice(&[0; 2]);
            body.extend_from_slice(&7_i32.to_le_bytes());
            (
                RequestHeader {
                    opcode: 12,
                    data: 0,
                    length_units: 4,
                },
                body,
            )
        }
        StructureRequest::Reparent => {
            let mut body = Vec::with_capacity(12);
            body.extend_from_slice(&CHILD_WINDOW.0.to_le_bytes());
            body.extend_from_slice(&NEW_PARENT_WINDOW.0.to_le_bytes());
            body.extend_from_slice(&0_i16.to_le_bytes());
            body.extend_from_slice(&0_i16.to_le_bytes());
            (
                RequestHeader {
                    opcode: 7,
                    data: 0,
                    length_units: 4,
                },
                body,
            )
        }
        StructureRequest::MapSubwindows => (
            RequestHeader {
                opcode: 9,
                data: 0,
                length_units: 2,
            },
            PARENT_WINDOW.0.to_le_bytes().to_vec(),
        ),
        StructureRequest::UnmapSubwindows => (
            RequestHeader {
                opcode: 11,
                data: 0,
                length_units: 2,
            },
            PARENT_WINDOW.0.to_le_bytes().to_vec(),
        ),
        StructureRequest::Circulate => (
            RequestHeader {
                opcode: 13,
                data: 0, // RaiseLowest
                length_units: 2,
            },
            PARENT_WINDOW.0.to_le_bytes().to_vec(),
        ),
    };
    fixture
        .request(NOTIFY_SENDER, SequenceNumber(41), header, &body)
        .expect("core window request through production dispatcher")
}

fn decode_structure_events(bytes: &[u8], order: ClientByteOrder) -> Vec<StructureEvent> {
    let read_u16 = |bytes: &[u8]| match order {
        ClientByteOrder::LittleEndian => u16::from_le_bytes(bytes.try_into().unwrap()),
        ClientByteOrder::BigEndian => u16::from_be_bytes(bytes.try_into().unwrap()),
    };
    let read_u32 = |bytes: &[u8]| match order {
        ClientByteOrder::LittleEndian => u32::from_le_bytes(bytes.try_into().unwrap()),
        ClientByteOrder::BigEndian => u32::from_be_bytes(bytes.try_into().unwrap()),
    };
    assert!(bytes.len().is_multiple_of(32), "complete event frames");
    bytes
        .chunks_exact(32)
        .map(|event| {
            assert_ne!(event[0], 0, "notification stream contains no X error");
            StructureEvent {
                event_type: event[0],
                sequence: read_u16(&event[2..4]),
                delivery_window: ResourceId(read_u32(&event[4..8])),
                affected_window: ResourceId(read_u32(&event[8..12])),
                reparent: (event[0] == 21).then(|| ResourceId(read_u32(&event[12..16]))),
            }
        })
        .collect()
}

fn filter_structure_frames(
    bytes: &[u8],
    order: ClientByteOrder,
    denied_window: ResourceId,
) -> Vec<u8> {
    let read_window = |event: &[u8]| match order {
        ClientByteOrder::LittleEndian => u32::from_le_bytes(event[4..8].try_into().unwrap()),
        ClientByteOrder::BigEndian => u32::from_be_bytes(event[4..8].try_into().unwrap()),
    };
    bytes
        .chunks_exact(32)
        .filter(|event| read_window(event) != denied_window.0)
        .flatten()
        .copied()
        .collect()
}

type StructureSnapshotRow = (
    ResourceId,
    ResourceId,
    i16,
    i16,
    u16,
    u16,
    MapState,
    Vec<ResourceId>,
);

fn structure_snapshot(fixture: &NotifyFixture) -> Vec<StructureSnapshotRow> {
    [
        PARENT_WINDOW,
        NEW_PARENT_WINDOW,
        CHILD_WINDOW,
        CIRCULATE_SIBLING,
    ]
    .into_iter()
    .filter_map(|id| {
        fixture.state.resources.window(id).map(|window| {
            (
                id,
                window.parent,
                window.x,
                window.y,
                window.width,
                window.height,
                window.map_state,
                window.children.clone(),
            )
        })
    })
    .collect()
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

// Mutations: use W(affected child) for every delivery, suppress every event
// for a denied client after its first denial, or describe the wrong core type
// at any migrated N05–N12/N14–N17/N19–N20/N22–N27/N29–N30 emitter.
#[test]
fn receive_structure_subject_is_delivery_window() {
    const CREATE: [StructureDelivery; 1] = [StructureDelivery {
        id: "N12",
        event_type: 16,
        delivery_window: PARENT_WINDOW,
        affected_window: CHILD_WINDOW,
        reparent: None,
    }];
    const MAP: [StructureDelivery; 2] = [
        StructureDelivery {
            id: "N19",
            event_type: 19,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N20",
            event_type: 19,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    const UNMAP: [StructureDelivery; 2] = [
        StructureDelivery {
            id: "N24",
            event_type: 18,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N25",
            event_type: 18,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    const CONFIGURE: [StructureDelivery; 2] = [
        StructureDelivery {
            id: "N14",
            event_type: 22,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N15",
            event_type: 22,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    const REPARENT: [StructureDelivery; 7] = [
        StructureDelivery {
            id: "N05",
            event_type: 18,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N06",
            event_type: 18,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N07",
            event_type: 21,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(NEW_PARENT_WINDOW),
        },
        StructureDelivery {
            id: "N08",
            event_type: 21,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(NEW_PARENT_WINDOW),
        },
        StructureDelivery {
            id: "N09",
            event_type: 21,
            delivery_window: NEW_PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(NEW_PARENT_WINDOW),
        },
        StructureDelivery {
            id: "N10",
            event_type: 19,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N11",
            event_type: 19,
            delivery_window: NEW_PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    const MAP_SUBWINDOWS: [StructureDelivery; 2] = [
        StructureDelivery {
            id: "N22",
            event_type: 19,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N23",
            event_type: 19,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    const UNMAP_SUBWINDOWS: [StructureDelivery; 2] = [
        StructureDelivery {
            id: "N26",
            event_type: 18,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N27",
            event_type: 18,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    const CIRCULATE: [StructureDelivery; 2] = [
        StructureDelivery {
            id: "N29",
            event_type: 26,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureDelivery {
            id: "N30",
            event_type: 26,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    let cases = [
        StructureCase {
            name: "CreateWindow",
            request: StructureRequest::Create,
            opcode: 1,
            deny_window: PARENT_WINDOW,
            deliveries: &CREATE,
        },
        StructureCase {
            name: "MapWindow",
            request: StructureRequest::Map,
            opcode: 8,
            deny_window: PARENT_WINDOW,
            deliveries: &MAP,
        },
        StructureCase {
            name: "UnmapWindow",
            request: StructureRequest::Unmap,
            opcode: 10,
            deny_window: PARENT_WINDOW,
            deliveries: &UNMAP,
        },
        StructureCase {
            name: "ConfigureWindow",
            request: StructureRequest::Configure,
            opcode: 12,
            deny_window: PARENT_WINDOW,
            deliveries: &CONFIGURE,
        },
        StructureCase {
            name: "ReparentWindow",
            request: StructureRequest::Reparent,
            opcode: 7,
            deny_window: NEW_PARENT_WINDOW,
            deliveries: &REPARENT,
        },
        StructureCase {
            name: "MapSubwindows",
            request: StructureRequest::MapSubwindows,
            opcode: 9,
            deny_window: PARENT_WINDOW,
            deliveries: &MAP_SUBWINDOWS,
        },
        StructureCase {
            name: "UnmapSubwindows",
            request: StructureRequest::UnmapSubwindows,
            opcode: 11,
            deny_window: PARENT_WINDOW,
            deliveries: &UNMAP_SUBWINDOWS,
        },
        StructureCase {
            name: "CirculateWindow",
            request: StructureRequest::Circulate,
            opcode: 13,
            deny_window: PARENT_WINDOW,
            deliveries: &CIRCULATE,
        },
    ];

    for case in cases {
        let mut baseline = structure_fixture(case.request);
        assert!(
            matches!(
                structure_request(&mut baseline, case.request),
                RequestOutcome::Handled
            ),
            "{} baseline outcome",
            case.name
        );
        let baseline_denied = baseline.drain(NOTIFY_RECEIVER_DENIED);
        let baseline_allowed = baseline.drain(NOTIFY_RECEIVER_ALLOWED);

        let mut allow = structure_fixture(case.request);
        allow.install_policy(|_, _| Decision::Allow);
        assert!(
            matches!(
                structure_request(&mut allow, case.request),
                RequestOutcome::Handled
            ),
            "{} allow outcome",
            case.name
        );
        let allow_denied = allow.drain(NOTIFY_RECEIVER_DENIED);
        let allow_allowed = allow.drain(NOTIFY_RECEIVER_ALLOWED);
        assert_eq!(
            allow_denied, baseline_denied,
            "{} no-policy denied bytes",
            case.name
        );
        assert_eq!(
            allow_allowed, baseline_allowed,
            "{} no-policy allowed bytes",
            case.name
        );
        assert_eq!(structure_snapshot(&allow), structure_snapshot(&baseline));
        assert_eq!(allow.backend.calls(), baseline.backend.calls());

        let mut denied = structure_fixture(case.request);
        let calls = denied.install_policy(move |_, call| {
            if call.hook == ExtensionHook::Receive
                && call.client == NOTIFY_RECEIVER_DENIED
                && call.receive == Some(EventSubject::Window(case.deny_window))
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        });
        assert!(
            matches!(
                structure_request(&mut denied, case.request),
                RequestOutcome::Handled
            ),
            "{} denied outcome",
            case.name
        );
        assert!(
            denied.drain(NOTIFY_SENDER).is_empty(),
            "{} no sender bytes",
            case.name
        );
        let denied_bytes = denied.drain(NOTIFY_RECEIVER_DENIED);
        let allowed_bytes = denied.drain(NOTIFY_RECEIVER_ALLOWED);

        let expected_allowed: Vec<_> = case
            .deliveries
            .iter()
            .map(|delivery| StructureEvent {
                event_type: delivery.event_type,
                sequence: 0x5678,
                delivery_window: delivery.delivery_window,
                affected_window: delivery.affected_window,
                reparent: delivery.reparent,
            })
            .collect();
        let expected_denied: Vec<_> = case
            .deliveries
            .iter()
            .filter(|delivery| delivery.delivery_window != case.deny_window)
            .map(|delivery| StructureEvent {
                event_type: delivery.event_type,
                sequence: 0x1234,
                delivery_window: delivery.delivery_window,
                affected_window: delivery.affected_window,
                reparent: delivery.reparent,
            })
            .collect();
        assert_eq!(
            decode_structure_events(&allow_allowed, ClientByteOrder::LittleEndian),
            expected_allowed,
            "{} allow stream and event ordering; IDs {:?}",
            case.name,
            case.deliveries.iter().map(|d| d.id).collect::<Vec<_>>()
        );
        assert_eq!(
            decode_structure_events(&denied_bytes, ClientByteOrder::BigEndian),
            expected_denied,
            "{} denied only at delivery window; IDs {:?}",
            case.name,
            case.deliveries.iter().map(|d| d.id).collect::<Vec<_>>()
        );
        assert_eq!(
            allowed_bytes, allow_allowed,
            "{} allowed bytes are isolated",
            case.name
        );
        assert_eq!(
            denied_bytes,
            filter_structure_frames(
                &baseline_denied,
                ClientByteOrder::BigEndian,
                case.deny_window
            ),
            "{} surviving denied-client events remain byte-identical",
            case.name
        );
        assert_eq!(structure_snapshot(&denied), structure_snapshot(&allow));
        assert_eq!(denied.backend.calls(), allow.backend.calls());
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            assert!(denied.state.clients.contains_key(&client.0));
        }

        let log = calls.lock().unwrap();
        assert!(!log.iter().any(|call| call.hook == ExtensionHook::Send));
        let receive_calls = receives(&log);
        assert_eq!(
            receive_calls.len(),
            case.deliveries.len() * 2,
            "{} hook count",
            case.name
        );
        for (index, delivery) in case.deliveries.iter().enumerate() {
            let group = &receive_calls[index * 2..index * 2 + 2];
            let mut clients: Vec<_> = group.iter().map(|call| call.client).collect();
            clients.sort_by_key(|client| client.0);
            assert_eq!(
                clients,
                [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED],
                "{} {} recipients",
                case.name,
                delivery.id
            );
            for call in group {
                assert_eq!(
                    call.receive,
                    Some(EventSubject::Window(delivery.delivery_window)),
                    "{} {} subject",
                    case.name,
                    delivery.id
                );
                assert_eq!(
                    call.events,
                    Some(vec![EventDescription::core(delivery.event_type)]),
                    "{} {} type",
                    case.name,
                    delivery.id
                );
                assert_eq!(call.request, request_opcodes(case.opcode, 0));
            }
        }
    }
}

// Mutation: drop N08's old-parent delivery or duplicate N09's new-parent
// delivery in the same-parent case.
#[test]
fn receive_reparent_preserves_three_targets_and_order() {
    fn same_parent_request(fixture: &mut NotifyFixture) -> RequestOutcome {
        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&CHILD_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&PARENT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&5_i16.to_le_bytes());
        body.extend_from_slice(&6_i16.to_le_bytes());
        fixture
            .request(
                NOTIFY_SENDER,
                SequenceNumber(43),
                RequestHeader {
                    opcode: 7,
                    data: 0,
                    length_units: 4,
                },
                &body,
            )
            .expect("same-parent ReparentWindow through production dispatcher")
    }

    let mut allow = structure_fixture(StructureRequest::Reparent);
    allow.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        structure_request(&mut allow, StructureRequest::Reparent),
        RequestOutcome::Handled
    ));
    let expected_allow = vec![
        StructureEvent {
            event_type: 18,
            sequence: 0x5678,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureEvent {
            event_type: 18,
            sequence: 0x5678,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureEvent {
            event_type: 21,
            sequence: 0x5678,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(NEW_PARENT_WINDOW),
        },
        StructureEvent {
            event_type: 21,
            sequence: 0x5678,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(NEW_PARENT_WINDOW),
        },
        StructureEvent {
            event_type: 21,
            sequence: 0x5678,
            delivery_window: NEW_PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(NEW_PARENT_WINDOW),
        },
        StructureEvent {
            event_type: 19,
            sequence: 0x5678,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureEvent {
            event_type: 19,
            sequence: 0x5678,
            delivery_window: NEW_PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    let allow_denied_client = allow.drain(NOTIFY_RECEIVER_DENIED);
    let allow_allowed_client = allow.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        decode_structure_events(&allow_allowed_client, ClientByteOrder::LittleEndian),
        expected_allow
    );
    let expected_allow_denied: Vec<_> = expected_allow
        .iter()
        .map(|event| StructureEvent {
            sequence: 0x1234,
            event_type: event.event_type,
            delivery_window: event.delivery_window,
            affected_window: event.affected_window,
            reparent: event.reparent,
        })
        .collect();
    assert_eq!(
        decode_structure_events(&allow_denied_client, ClientByteOrder::BigEndian),
        expected_allow_denied
    );

    let mut denied = structure_fixture(StructureRequest::Reparent);
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(NEW_PARENT_WINDOW))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(
        structure_request(&mut denied, StructureRequest::Reparent),
        RequestOutcome::Handled
    ));
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    let denied_bytes = denied.drain(NOTIFY_RECEIVER_DENIED);
    let allowed_bytes = denied.drain(NOTIFY_RECEIVER_ALLOWED);
    let expected_denied: Vec<_> = expected_allow
        .iter()
        .filter(|event| event.delivery_window != NEW_PARENT_WINDOW)
        .map(|event| StructureEvent {
            sequence: 0x1234,
            event_type: event.event_type,
            delivery_window: event.delivery_window,
            affected_window: event.affected_window,
            reparent: event.reparent,
        })
        .collect();
    assert_eq!(
        decode_structure_events(&denied_bytes, ClientByteOrder::BigEndian),
        expected_denied
    );
    assert_eq!(allowed_bytes, allow_allowed_client);
    assert_eq!(structure_snapshot(&denied), structure_snapshot(&allow));
    assert_eq!(denied.backend.calls(), allow.backend.calls());
    assert_eq!(receives(&calls.lock().unwrap()).len(), 14);

    let same_parent_events = vec![
        StructureEvent {
            event_type: 18,
            sequence: 0x5678,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureEvent {
            event_type: 18,
            sequence: 0x5678,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureEvent {
            event_type: 21,
            sequence: 0x5678,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(PARENT_WINDOW),
        },
        StructureEvent {
            event_type: 21,
            sequence: 0x5678,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: Some(PARENT_WINDOW),
        },
        StructureEvent {
            event_type: 19,
            sequence: 0x5678,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
        StructureEvent {
            event_type: 19,
            sequence: 0x5678,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        },
    ];
    let mut same_allow = structure_fixture(StructureRequest::Reparent);
    let same_allow_calls = same_allow.install_policy(|_, _| Decision::Allow);
    assert!(matches!(
        same_parent_request(&mut same_allow),
        RequestOutcome::Handled
    ));
    let same_allow_denied_bytes = same_allow.drain(NOTIFY_RECEIVER_DENIED);
    let same_allow_allowed_bytes = same_allow.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        decode_structure_events(&same_allow_denied_bytes, ClientByteOrder::BigEndian),
        same_parent_events
            .iter()
            .map(|event| StructureEvent {
                sequence: 0x1234,
                event_type: event.event_type,
                delivery_window: event.delivery_window,
                affected_window: event.affected_window,
                reparent: event.reparent,
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        decode_structure_events(&same_allow_allowed_bytes, ClientByteOrder::LittleEndian),
        same_parent_events
    );
    let same_allow_log = same_allow_calls.lock().unwrap();
    assert_eq!(receives(&same_allow_log).len(), 12);
    assert!(
        !receives(&same_allow_log)
            .iter()
            .any(|call| call.receive == Some(EventSubject::Window(NEW_PARENT_WINDOW)))
    );

    let mut same_denied = structure_fixture(StructureRequest::Reparent);
    let same_denied_calls = same_denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(PARENT_WINDOW))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(
        same_parent_request(&mut same_denied),
        RequestOutcome::Handled
    ));
    let same_denied_bytes = same_denied.drain(NOTIFY_RECEIVER_DENIED);
    let same_denied_allowed = same_denied.drain(NOTIFY_RECEIVER_ALLOWED);
    let expected_same_denied = same_parent_events
        .iter()
        .filter(|event| event.delivery_window != PARENT_WINDOW)
        .map(|event| StructureEvent {
            sequence: 0x1234,
            event_type: event.event_type,
            delivery_window: event.delivery_window,
            affected_window: event.affected_window,
            reparent: event.reparent,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decode_structure_events(&same_denied_bytes, ClientByteOrder::BigEndian),
        expected_same_denied
    );
    assert_eq!(same_denied_allowed, same_allow_allowed_bytes);
    assert_eq!(
        structure_snapshot(&same_denied),
        structure_snapshot(&same_allow)
    );
    assert_eq!(same_denied.backend.calls(), same_allow.backend.calls());
    assert_eq!(receives(&same_denied_calls.lock().unwrap()).len(), 12);
}

// Mutation: skip child gravity application when Receive denies GravityNotify.
#[test]
fn receive_gravity_denial_keeps_child_move() {
    const HOST_PARENT: u32 = 0x0055_0001;
    const HOST_CHILD: u32 = 0x0055_0002;

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
            10,
            10,
            None,
        );
        let parent = fixture.state.resources.window_mut(PARENT_WINDOW).unwrap();
        parent.map_state = MapState::Viewable;
        parent.host_xid = Some(WindowHandle::from_raw_for_test(HOST_PARENT));
        let child = fixture.state.resources.window_mut(CHILD_WINDOW).unwrap();
        child.map_state = MapState::Viewable;
        child.win_gravity = 8; // SouthGravity
        child.host_xid = Some(WindowHandle::from_raw_for_test(HOST_CHILD));
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            let masks = &mut fixture
                .state
                .clients
                .get_mut(&client.0)
                .unwrap()
                .event_masks;
            if client == NOTIFY_RECEIVER_DENIED {
                masks.insert(CHILD_WINDOW, 0x0002_0000);
            } else {
                masks.insert(PARENT_WINDOW, 0x0008_0000);
            }
        }
        let denied = fixture
            .state
            .clients
            .get_mut(&NOTIFY_RECEIVER_DENIED.0)
            .expect("denied receiver");
        denied.byte_order = ClientByteOrder::BigEndian;
        denied
            .last_sequence
            .store(0x1234, std::sync::atomic::Ordering::Relaxed);
        fixture
            .state
            .clients
            .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
            .expect("allowed receiver")
            .last_sequence
            .store(0x5678, std::sync::atomic::Ordering::Relaxed);
        fixture
    }

    fn resize_parent(fixture: &mut NotifyFixture) -> RequestOutcome {
        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&PARENT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&8_u16.to_le_bytes()); // CWHeight
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&120_u32.to_le_bytes());
        fixture
            .request(
                NOTIFY_SENDER,
                SequenceNumber(42),
                RequestHeader {
                    opcode: 12,
                    data: 0,
                    length_units: 4,
                },
                &body,
            )
            .expect("ConfigureWindow through production dispatcher")
    }

    fn child_position(fixture: &NotifyFixture) -> (i16, i16) {
        let child = fixture.state.resources.window(CHILD_WINDOW).unwrap();
        (child.x, child.y)
    }

    let mut allow = fixture();
    allow.install_policy(|_, _| Decision::Allow);
    assert!(matches!(resize_parent(&mut allow), RequestOutcome::Handled));
    let allow_denied = allow.drain(NOTIFY_RECEIVER_DENIED);
    let allow_allowed = allow.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        decode_structure_events(&allow_denied, ClientByteOrder::BigEndian),
        vec![StructureEvent {
            event_type: 24,
            sequence: 0x1234,
            delivery_window: CHILD_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        }]
    );
    assert_eq!(
        decode_structure_events(&allow_allowed, ClientByteOrder::LittleEndian),
        vec![StructureEvent {
            event_type: 24,
            sequence: 0x5678,
            delivery_window: PARENT_WINDOW,
            affected_window: CHILD_WINDOW,
            reparent: None,
        }]
    );
    assert_eq!(child_position(&allow), (10, 40));
    assert!(allow.backend.calls().iter().any(|call| matches!(
        call,
        RecordedCall::MoveSubwindowsForGravity { moves, .. }
            if moves == &vec![(HOST_CHILD, 10, 40)]
    )));

    let mut denied = fixture();
    let calls = denied.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(CHILD_WINDOW))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    });
    assert!(matches!(
        resize_parent(&mut denied),
        RequestOutcome::Handled
    ));
    assert_eq!(
        child_position(&denied),
        child_position(&allow),
        "GravityNotify denial must not suppress the child move"
    );
    assert_eq!(
        denied.backend.calls(),
        allow.backend.calls(),
        "GravityNotify denial must not suppress backend movement"
    );
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert!(denied.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    let denied_allowed = denied.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(denied_allowed, allow_allowed);
    assert_eq!(child_position(&denied), child_position(&allow));
    assert_eq!(denied.backend.calls(), allow.backend.calls());
    assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));
    let log = calls.lock().unwrap();
    let receive_calls = receives(&log);
    assert_eq!(receive_calls.len(), 2);
    assert_eq!(receive_calls[0].client, NOTIFY_RECEIVER_DENIED);
    assert_eq!(
        receive_calls[0].receive,
        Some(EventSubject::Window(CHILD_WINDOW))
    );
    assert_eq!(receive_calls[1].client, NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        receive_calls[1].receive,
        Some(EventSubject::Window(PARENT_WINDOW))
    );
    assert!(receive_calls.iter().all(|call| {
        call.events == Some(vec![EventDescription::core(24)])
            && call.request == request_opcodes(12, 0)
    }));
    assert!(!log.iter().any(|call| call.hook == ExtensionHook::Send));
}

// Mutation: move the Receive check ahead of the existing no-change exit.
#[test]
fn receive_structure_noop_does_not_ask() {
    let mut fixture = structure_fixture(StructureRequest::Unmap);
    fixture
        .state
        .resources
        .window_mut(CHILD_WINDOW)
        .expect("child")
        .map_state = MapState::Unmapped;
    let calls = fixture.install_policy(|_, _| Decision::Allow);

    // Put the child in the states that make each operation an existing no-op.
    fixture.state.resources.window_mut(CHILD_WINDOW).unwrap().x = 7;
    fixture
        .state
        .resources
        .window_mut(CHILD_WINDOW)
        .unwrap()
        .map_state = MapState::Viewable;
    assert!(matches!(
        structure_request(&mut fixture, StructureRequest::Configure),
        RequestOutcome::Handled
    ));
    fixture
        .state
        .resources
        .window_mut(CHILD_WINDOW)
        .unwrap()
        .map_state = MapState::Viewable;
    assert!(matches!(
        structure_request(&mut fixture, StructureRequest::Map),
        RequestOutcome::Handled
    ));
    fixture
        .state
        .resources
        .window_mut(CHILD_WINDOW)
        .unwrap()
        .map_state = MapState::Unmapped;
    assert!(matches!(
        structure_request(&mut fixture, StructureRequest::Unmap),
        RequestOutcome::Handled
    ));
    assert!(receives(&calls.lock().unwrap()).is_empty());
    assert!(fixture.drain(NOTIFY_SENDER).is_empty());
    assert!(fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert!(fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
}

const SUBSTRUCTURE_REDIRECT_MASK: u32 = 0x0010_0000;

#[derive(Clone, Copy)]
struct RedirectCase {
    name: &'static str,
    request: StructureRequest,
    request_opcode: u8,
    redirect_event: u8,
    ordinary_event: u8,
}

const REDIRECT_CASES: [RedirectCase; 3] = [
    RedirectCase {
        name: "MapWindow",
        request: StructureRequest::Map,
        request_opcode: 8,
        redirect_event: 20,
        ordinary_event: 19,
    },
    RedirectCase {
        name: "ConfigureWindow",
        request: StructureRequest::Configure,
        request_opcode: 12,
        redirect_event: 23,
        ordinary_event: 22,
    },
    RedirectCase {
        name: "CirculateWindow",
        request: StructureRequest::Circulate,
        request_opcode: 13,
        redirect_event: 27,
        ordinary_event: 26,
    },
];

fn redirect_fixture(
    case: RedirectCase,
    selector: Option<ClientId>,
    override_redirect: bool,
) -> NotifyFixture {
    let mut fixture = structure_fixture(case.request);
    if let Some(selector) = selector {
        fixture
            .state
            .clients
            .get_mut(&selector.0)
            .expect("redirect selector")
            .event_masks
            .insert(PARENT_WINDOW, SUBSTRUCTURE_REDIRECT_MASK);
    }
    fixture
        .state
        .resources
        .window_mut(CHILD_WINDOW)
        .expect("redirected child")
        .override_redirect = override_redirect;
    for (index, id) in [CHILD_WINDOW, CIRCULATE_SIBLING].into_iter().enumerate() {
        if let Some(window) = fixture.state.resources.window_mut(id) {
            window.host_xid = Some(WindowHandle::from_raw_for_test(
                0x0053_0010 + u32::try_from(index).unwrap(),
            ));
        }
    }
    fixture
}

fn assert_redirect_frame(bytes: &[u8], case: RedirectCase, order: ClientByteOrder) {
    assert_eq!(
        bytes.len(),
        32,
        "{} emits exactly one redirect event",
        case.name
    );
    assert_eq!(bytes[0], case.redirect_event, "{} event code", case.name);
    let read_u32 = |slice: &[u8]| match order {
        ClientByteOrder::LittleEndian => u32::from_le_bytes(slice.try_into().unwrap()),
        ClientByteOrder::BigEndian => u32::from_be_bytes(slice.try_into().unwrap()),
    };
    assert_eq!(
        read_u32(&bytes[4..8]),
        PARENT_WINDOW.0,
        "{} parent",
        case.name
    );
    assert_eq!(
        read_u32(&bytes[8..12]),
        CHILD_WINDOW.0,
        "{} target",
        case.name
    );
}

fn event_types(bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len().is_multiple_of(32), "complete event frames");
    bytes.chunks_exact(32).map(|event| event[0]).collect()
}

// Mutation per row: after a checked redirect recipient gets no bytes, continue
// into MapWindow, ConfigureWindow, or CirculateWindow; the pre-state and
// ordinary-notification assertions below must fail for that row.
#[test]
fn receive_redirect_denial_preserves_redirection() {
    for case in REDIRECT_CASES {
        let mut baseline = redirect_fixture(case, Some(NOTIFY_RECEIVER_DENIED), false);
        let before = structure_snapshot(&baseline);
        let backend_before = baseline.backend.calls();
        assert!(matches!(
            structure_request(&mut baseline, case.request),
            RequestOutcome::Handled
        ));
        let baseline_redirect = baseline.drain(NOTIFY_RECEIVER_DENIED);
        assert_redirect_frame(&baseline_redirect, case, ClientByteOrder::BigEndian);
        assert!(baseline.drain(NOTIFY_SENDER).is_empty());
        assert!(baseline.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        assert_eq!(
            structure_snapshot(&baseline),
            before,
            "{} baseline state",
            case.name
        );
        assert_eq!(
            baseline.backend.calls(),
            backend_before,
            "{} baseline backend",
            case.name
        );

        let mut allow = redirect_fixture(case, Some(NOTIFY_RECEIVER_DENIED), false);
        allow.install_policy(|_, _| Decision::Allow);
        assert!(matches!(
            structure_request(&mut allow, case.request),
            RequestOutcome::Handled
        ));
        let allow_redirect = allow.drain(NOTIFY_RECEIVER_DENIED);
        assert_eq!(
            allow_redirect, baseline_redirect,
            "{} allow wire parity",
            case.name
        );
        assert!(allow.drain(NOTIFY_SENDER).is_empty());
        assert!(allow.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        assert_eq!(
            structure_snapshot(&allow),
            before,
            "{} allow state",
            case.name
        );
        assert_eq!(
            allow.backend.calls(),
            backend_before,
            "{} allow backend",
            case.name
        );

        let mut denied = redirect_fixture(case, Some(NOTIFY_RECEIVER_DENIED), false);
        let calls = denied.install_policy(move |_, call| {
            if call.hook == ExtensionHook::Receive
                && call.client == NOTIFY_RECEIVER_DENIED
                && call.receive == Some(EventSubject::Window(PARENT_WINDOW))
                && call.events == Some(vec![EventDescription::core(case.redirect_event)])
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        });
        assert!(matches!(
            structure_request(&mut denied, case.request),
            RequestOutcome::Handled
        ));
        assert!(
            denied.drain(NOTIFY_SENDER).is_empty(),
            "{} no request error",
            case.name
        );
        assert!(
            denied.drain(NOTIFY_RECEIVER_DENIED).is_empty(),
            "{} denied selector receives no event bytes",
            case.name
        );
        assert!(
            denied.drain(NOTIFY_RECEIVER_ALLOWED).is_empty(),
            "{} ordinary listeners receive no operation notification",
            case.name
        );
        assert_eq!(
            structure_snapshot(&denied),
            before,
            "{} denied state",
            case.name
        );
        assert_eq!(structure_snapshot(&denied), structure_snapshot(&allow));
        assert_eq!(
            denied.backend.calls(),
            backend_before,
            "{} denied backend",
            case.name
        );
        assert_eq!(denied.backend.calls(), allow.backend.calls());
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            assert!(
                denied.state.clients.contains_key(&client.0),
                "{} stays connected",
                case.name
            );
        }

        let log = calls.lock().unwrap();
        assert!(!log.iter().any(|call| call.hook == ExtensionHook::Send));
        let receive_calls = receives(&log);
        assert_eq!(receive_calls.len(), 1, "{} one legal selector", case.name);
        let call = receive_calls[0];
        assert_eq!(call.client, NOTIFY_RECEIVER_DENIED);
        assert_eq!(call.receive, Some(EventSubject::Window(PARENT_WINDOW)));
        assert_eq!(
            call.events,
            Some(vec![EventDescription::core(case.redirect_event)])
        );
        assert_eq!(call.request, request_opcodes(case.request_opcode, 0));
    }
}

// Mutation: manufacture a redirect Receive check from a mask bit before the
// existing requester, override-redirect, and no-selector filters.
#[test]
fn receive_redirect_does_not_check_unselected_clients() {
    let cases = [
        // The requester owns the only redirect selection; its request is not redirected.
        (REDIRECT_CASES[1], Some(NOTIFY_SENDER), false),
        // A different client selected the parent, but override_redirect bypasses it.
        (REDIRECT_CASES[0], Some(NOTIFY_RECEIVER_DENIED), true),
        // No redirect selection exists; CirculateWindow performs its normal operation.
        (REDIRECT_CASES[2], None, false),
    ];

    for (case, selector, override_redirect) in cases {
        let mut baseline = redirect_fixture(case, selector, override_redirect);
        let before = structure_snapshot(&baseline);
        assert!(matches!(
            structure_request(&mut baseline, case.request),
            RequestOutcome::Handled
        ));
        let baseline_denied = baseline.drain(NOTIFY_RECEIVER_DENIED);
        let baseline_allowed = baseline.drain(NOTIFY_RECEIVER_ALLOWED);
        assert!(baseline.drain(NOTIFY_SENDER).is_empty());
        assert_ne!(
            structure_snapshot(&baseline),
            before,
            "{} operation proceeds",
            case.name
        );
        let mut baseline_event_types = event_types(&baseline_denied);
        baseline_event_types.extend(event_types(&baseline_allowed));
        assert!(
            !baseline_event_types.contains(&case.redirect_event),
            "{} no redirect notification is manufactured",
            case.name
        );
        assert!(
            baseline_event_types.contains(&case.ordinary_event),
            "{} ordinary notification is preserved",
            case.name
        );

        let mut observed = redirect_fixture(case, selector, override_redirect);
        let calls = observed.install_policy(|_, _| Decision::Allow);
        assert!(matches!(
            structure_request(&mut observed, case.request),
            RequestOutcome::Handled
        ));
        assert!(observed.drain(NOTIFY_SENDER).is_empty());
        assert_eq!(
            observed.drain(NOTIFY_RECEIVER_DENIED),
            baseline_denied,
            "{} denied listener",
            case.name
        );
        assert_eq!(
            observed.drain(NOTIFY_RECEIVER_ALLOWED),
            baseline_allowed,
            "{} allowed listener",
            case.name
        );
        assert_eq!(structure_snapshot(&observed), structure_snapshot(&baseline));
        assert_eq!(observed.backend.calls(), baseline.backend.calls());
        let log = calls.lock().unwrap();
        assert!(!log.iter().any(|call| call.hook == ExtensionHook::Send));
        let receive_calls = receives(&log);
        assert!(
            receive_calls.iter().all(|call| {
                call.events != Some(vec![EventDescription::core(case.redirect_event)])
            }),
            "{} never checks a redirect event with no selected redirect recipient",
            case.name
        );
        assert!(
            receive_calls
                .iter()
                .all(|call| call.request == request_opcodes(case.request_opcode, 0)),
            "{} ordinary notifications keep request context",
            case.name
        );
    }
}

// Mutation: cache the selector's first Allow across revocation, or let a later
// denied delivery fall through into the requested operation.
#[test]
fn receive_redirect_revocation_still_redirects() {
    for case in REDIRECT_CASES {
        let mut fixture = redirect_fixture(case, Some(NOTIFY_RECEIVER_DENIED), false);
        let before = structure_snapshot(&fixture);
        let backend_before = fixture.backend.calls();
        let revoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let policy_revoked = std::sync::Arc::clone(&revoked);
        let calls = fixture.install_policy(move |_, call| {
            if call.hook == ExtensionHook::Receive
                && call.client == NOTIFY_RECEIVER_DENIED
                && call.receive == Some(EventSubject::Window(PARENT_WINDOW))
                && call.events == Some(vec![EventDescription::core(case.redirect_event)])
                && policy_revoked.load(std::sync::atomic::Ordering::Relaxed)
            {
                Decision::Deny(DENY)
            } else {
                Decision::Allow
            }
        });

        assert!(matches!(
            structure_request(&mut fixture, case.request),
            RequestOutcome::Handled
        ));
        let first = fixture.drain(NOTIFY_RECEIVER_DENIED);
        assert_redirect_frame(&first, case, ClientByteOrder::BigEndian);
        assert!(fixture.drain(NOTIFY_SENDER).is_empty());
        assert!(fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        assert_eq!(
            structure_snapshot(&fixture),
            before,
            "{} first request stays redirected",
            case.name
        );
        assert_eq!(
            fixture.backend.calls(),
            backend_before,
            "{} first backend",
            case.name
        );

        revoked.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            structure_request(&mut fixture, case.request),
            RequestOutcome::Handled
        ));
        assert!(
            fixture.drain(NOTIFY_SENDER).is_empty(),
            "{} no request error",
            case.name
        );
        assert!(
            fixture.drain(NOTIFY_RECEIVER_DENIED).is_empty(),
            "{} revoked selector receives no second event",
            case.name
        );
        assert!(fixture.drain(NOTIFY_RECEIVER_ALLOWED).is_empty());
        assert_eq!(
            structure_snapshot(&fixture),
            before,
            "{} second request stays redirected",
            case.name
        );
        assert_eq!(
            fixture.backend.calls(),
            backend_before,
            "{} second backend",
            case.name
        );
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            assert!(
                fixture.state.clients.contains_key(&client.0),
                "{} stays connected after revocation",
                case.name
            );
        }

        let log = calls.lock().unwrap();
        assert!(!log.iter().any(|call| call.hook == ExtensionHook::Send));
        let receive_calls = receives(&log);
        assert_eq!(
            receive_calls.len(),
            2,
            "{} rechecks after revocation",
            case.name
        );
        for call in receive_calls {
            assert_eq!(call.client, NOTIFY_RECEIVER_DENIED);
            assert_eq!(call.receive, Some(EventSubject::Window(PARENT_WINDOW)));
            assert_eq!(
                call.events,
                Some(vec![EventDescription::core(case.redirect_event)])
            );
            assert_eq!(call.request, request_opcodes(case.request_opcode, 0));
        }
    }
}

// Keep existing fixture tests private while letting the Task 8 parity test
// rerun these established no-policy/Allow comparisons from its sibling module.
pub(super) fn run_no_policy_parity_fixtures() {
    receive_host_expose_checks_each_delivery_window();
    receive_clear_area_expose_denial_keeps_paint();
    receive_map_visibility_denial_keeps_descendant_walk();
    receive_expose_rectangles_keep_counts_and_collapse();
    receive_structure_subject_is_delivery_window();
    receive_reparent_preserves_three_targets_and_order();
    receive_gravity_denial_keeps_child_move();
    receive_structure_noop_does_not_ask();
    receive_redirect_denial_preserves_redirection();
    receive_redirect_does_not_check_unselected_clients();
    receive_redirect_revocation_still_redirects();
}
