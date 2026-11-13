use super::{
    access_notify_support::*,
    extensions::{ExtensionHook, RecordedExtensionCheck},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    backend::WindowHandle,
    core_loop::process_disconnect::process_disconnect,
    resources::{COMPOSITE_OVERLAY_WINDOW, MapState, ROOT_WINDOW},
    server::{
        ActiveKeyboardGrab, ActiveKeyboardGrabSource, ActivePointerGrab, CoreFocus, SelectionOwner,
    },
};

const DENY: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0,
};
const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
const SUBSTRUCTURE_NOTIFY: u32 = 0x0008_0000;
const DESTROY_PARENT: ResourceId = ResourceId(0x0053_0010);
const DESTROY_CHILD: ResourceId = ResourceId(0x0053_0011);
const DESTROY_GRANDCHILD: ResourceId = ResourceId(0x0053_0012);

fn receives(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .collect()
}

fn received_sites(calls: &[&RecordedExtensionCheck], client: ClientId) -> Vec<(EventSubject, u8)> {
    calls
        .iter()
        .filter(|call| call.client == client)
        .map(|call| {
            let subject = call.receive.expect("Receive check has a subject");
            let event = call
                .events
                .as_ref()
                .expect("Receive check has event metadata");
            assert_eq!(
                event.len(),
                1,
                "each destruction delivery is described alone"
            );
            (subject, event[0].type_code)
        })
        .collect()
}

fn event_tuples(bytes: &[u8]) -> Vec<(u8, u32, u32)> {
    assert_eq!(bytes.len() % 32, 0, "whole 32-byte protocol packets");
    bytes
        .chunks_exact(32)
        .filter(|packet| packet[0] != 1 && packet[0] != 0)
        .map(|event| {
            (
                event[0] & 0x7f,
                u32::from_le_bytes(event[4..8].try_into().unwrap()),
                u32::from_le_bytes(event[8..12].try_into().unwrap()),
            )
        })
        .collect()
}

fn destroy_fixture() -> NotifyFixture {
    let mut fixture = NotifyFixture::new();
    for (window, parent, host_xid) in [
        (DESTROY_PARENT, ROOT_WINDOW, 0x0054_0010),
        (DESTROY_CHILD, DESTROY_PARENT, 0x0054_0011),
        (DESTROY_GRANDCHILD, DESTROY_CHILD, 0x0054_0012),
    ] {
        fixture.state.resources.create_window(
            NOTIFY_SENDER,
            CreateWindowRequest {
                depth: 24,
                window,
                parent,
                width: 80,
                height: 60,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let record = fixture
            .state
            .resources
            .window_mut(window)
            .expect("seeded window");
        record.host_xid = WindowHandle::from_raw(host_xid);
        record.map_state = MapState::Viewable;
    }

    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        let masks = &mut fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("receiver")
            .event_masks;
        for window in [DESTROY_PARENT, DESTROY_CHILD, DESTROY_GRANDCHILD] {
            masks.insert(window, STRUCTURE_NOTIFY);
        }
        for window in [DESTROY_PARENT, DESTROY_CHILD] {
            masks
                .entry(window)
                .and_modify(|mask| *mask |= SUBSTRUCTURE_NOTIFY)
                .or_insert(SUBSTRUCTURE_NOTIFY);
        }
    }
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("allowed receiver")
        .event_masks
        .insert(ROOT_WINDOW, SUBSTRUCTURE_NOTIFY);

    fixture.state.core_focus = CoreFocus {
        raw: DESTROY_GRANDCHILD.0,
        revert_to: 2,
        time: 0,
    };
    fixture.state.set_pointer_grab(ActivePointerGrab {
        owner: NOTIFY_RECEIVER_ALLOWED,
        grab_window: DESTROY_GRANDCHILD,
        event_mask: u16::MAX,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });
    fixture.state.active_keyboard_grab = Some(ActiveKeyboardGrab {
        owner: NOTIFY_RECEIVER_ALLOWED,
        grab_window: DESTROY_GRANDCHILD,
        source: ActiveKeyboardGrabSource::Explicit,
        owner_events: false,
        via_xi2: false,
        xi2_mask: 0,
    });
    let selection = fixture.state.atoms.intern("ACCESS_NOTIFY_LIFECYCLE", false);
    fixture.state.selections.insert(
        selection,
        SelectionOwner {
            window: DESTROY_GRANDCHILD,
            last_time_changed: 1,
            client: Some(NOTIFY_SENDER),
        },
    );
    fixture
}

fn run_destroy_request(fixture: &mut NotifyFixture, opcode: u8) {
    let body = DESTROY_PARENT.0.to_le_bytes();
    assert!(matches!(
        fixture
            .request(
                NOTIFY_SENDER,
                SequenceNumber(41),
                RequestHeader {
                    opcode,
                    data: 0,
                    length_units: 2,
                },
                &body,
            )
            .expect("destroy request through process_request"),
        RequestOutcome::Handled
    ));
}

fn assert_destroy_cleanup(fixture: &NotifyFixture, opcode: u8) {
    let destroyed = if opcode == 4 {
        vec![DESTROY_PARENT, DESTROY_CHILD, DESTROY_GRANDCHILD]
    } else {
        vec![DESTROY_CHILD, DESTROY_GRANDCHILD]
    };
    for window in &destroyed {
        assert!(
            fixture.state.resources.window(*window).is_none(),
            "destroyed resource {window:?} is gone"
        );
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            assert!(
                !fixture.state.clients[&client.0]
                    .event_masks
                    .contains_key(window),
                "subscriptions for {window:?} are removed"
            );
        }
    }
    if opcode == 5 {
        assert!(fixture.state.resources.window(DESTROY_PARENT).is_some());
    }
    assert!(fixture.state.active_pointer_grab.is_none());
    assert!(fixture.state.active_keyboard_grab.is_none());
    assert_ne!(fixture.state.core_focus.raw, DESTROY_GRANDCHILD.0);
    let selection = fixture
        .state
        .selections
        .values()
        .next()
        .expect("selection record remains");
    assert_eq!(selection.window, ResourceId(0));
    assert_eq!(selection.client, None);
}

fn install_destroy_policy(
    fixture: &mut NotifyFixture,
    deny: bool,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    fixture.install_policy(move |_, call| {
        if deny
            && call.hook == ExtensionHook::Receive
            && call.client == NOTIFY_RECEIVER_DENIED
            && call.receive == Some(EventSubject::Window(DESTROY_PARENT))
        {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    })
}

// Mutations: return from the destroy loop on denial; perform the receive
// check after deleting subscriptions/resources; use the top subject for every descendant.
#[test]
fn receive_destroy_denial_keeps_cleanup() {
    for opcode in [4, 5] {
        let mut baseline = destroy_fixture();
        assert!(!baseline.state.policies_installed());
        run_destroy_request(&mut baseline, opcode);
        let baseline_denied_stream = baseline.drain(NOTIFY_RECEIVER_DENIED);
        let baseline_allowed_stream = baseline.drain(NOTIFY_RECEIVER_ALLOWED);

        let mut control = destroy_fixture();
        install_destroy_policy(&mut control, false);
        run_destroy_request(&mut control, opcode);
        let control_denied_stream = control.drain(NOTIFY_RECEIVER_DENIED);
        let control_allowed_stream = control.drain(NOTIFY_RECEIVER_ALLOWED);
        assert_eq!(control_denied_stream, baseline_denied_stream);
        assert_eq!(control_allowed_stream, baseline_allowed_stream);
        let control_backend = control.backend.calls();
        assert!(control.drain(NOTIFY_SENDER).is_empty());
        assert_destroy_cleanup(&control, opcode);

        let mut denied = destroy_fixture();
        let calls = install_destroy_policy(&mut denied, true);
        run_destroy_request(&mut denied, opcode);
        let denied_stream = denied.drain(NOTIFY_RECEIVER_DENIED);
        let allowed_stream = denied.drain(NOTIFY_RECEIVER_ALLOWED);
        assert_eq!(allowed_stream, control_allowed_stream);
        assert_destroy_cleanup(&denied, opcode);
        assert_eq!(denied.backend.calls(), control_backend);
        assert_eq!(denied.state.selections, control.state.selections);
        for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
            assert_eq!(
                denied.state.clients[&client.0].event_masks,
                control.state.clients[&client.0].event_masks
            );
        }
        assert_eq!(
            (
                denied.state.core_focus.raw,
                denied.state.core_focus.revert_to,
                denied.state.core_focus.time,
            ),
            (
                control.state.core_focus.raw,
                control.state.core_focus.revert_to,
                control.state.core_focus.time,
            )
        );
        assert!(denied.state.clients.contains_key(&NOTIFY_RECEIVER_DENIED.0));
        assert!(!denied.state.clients[&NOTIFY_RECEIVER_DENIED.0].gone);
        assert!(!denied.state.clients[&NOTIFY_RECEIVER_DENIED.0].write_failed);
        assert!(
            denied.drain(NOTIFY_SENDER).is_empty(),
            "no X error to requestor"
        );

        let expected_denied = if opcode == 4 {
            vec![
                (17, DESTROY_GRANDCHILD.0, DESTROY_GRANDCHILD.0),
                (17, DESTROY_CHILD.0, DESTROY_GRANDCHILD.0),
                (17, DESTROY_CHILD.0, DESTROY_CHILD.0),
            ]
        } else {
            vec![
                (18, DESTROY_CHILD.0, DESTROY_CHILD.0),
                (17, DESTROY_GRANDCHILD.0, DESTROY_GRANDCHILD.0),
                (17, DESTROY_CHILD.0, DESTROY_GRANDCHILD.0),
                (17, DESTROY_CHILD.0, DESTROY_CHILD.0),
            ]
        };
        assert_eq!(event_tuples(&denied_stream), expected_denied);
        assert_eq!(
            event_tuples(&control_allowed_stream),
            if opcode == 4 {
                vec![
                    (18, DESTROY_PARENT.0, DESTROY_PARENT.0),
                    (18, ROOT_WINDOW.0, DESTROY_PARENT.0),
                    (17, DESTROY_GRANDCHILD.0, DESTROY_GRANDCHILD.0),
                    (17, DESTROY_CHILD.0, DESTROY_GRANDCHILD.0),
                    (17, DESTROY_CHILD.0, DESTROY_CHILD.0),
                    (17, DESTROY_PARENT.0, DESTROY_CHILD.0),
                    (17, DESTROY_PARENT.0, DESTROY_PARENT.0),
                    (17, ROOT_WINDOW.0, DESTROY_PARENT.0),
                ]
            } else {
                vec![
                    (18, DESTROY_CHILD.0, DESTROY_CHILD.0),
                    (18, DESTROY_PARENT.0, DESTROY_CHILD.0),
                    (17, DESTROY_GRANDCHILD.0, DESTROY_GRANDCHILD.0),
                    (17, DESTROY_CHILD.0, DESTROY_GRANDCHILD.0),
                    (17, DESTROY_CHILD.0, DESTROY_CHILD.0),
                    (17, DESTROY_PARENT.0, DESTROY_CHILD.0),
                ]
            }
        );
        let calls = calls.lock().unwrap();
        let receives = receives(&calls);
        let request = Some(RequestOpcodes {
            major: opcode,
            minor: 0,
        });
        assert!(receives.iter().all(|call| call.request == request));
        for subject_window in [DESTROY_CHILD, DESTROY_PARENT] {
            assert!(
                receives.iter().any(|call| {
                    call.receive == Some(EventSubject::Window(subject_window))
                        && call.owner == Some(NOTIFY_SENDER)
                }),
                "live owner resolves for {subject_window:?} before resource removal"
            );
        }
        let expected_sites = if opcode == 4 {
            vec![
                (DESTROY_PARENT, 18),
                (ROOT_WINDOW, 18),
                (DESTROY_GRANDCHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_PARENT, 17),
                (DESTROY_PARENT, 17),
                (ROOT_WINDOW, 17),
            ]
        } else {
            vec![
                (DESTROY_CHILD, 18),
                (DESTROY_PARENT, 18),
                (DESTROY_GRANDCHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_PARENT, 17),
            ]
        };
        let allowed_sites = received_sites(&receives, NOTIFY_RECEIVER_ALLOWED)
            .into_iter()
            .map(|(subject, event)| {
                let EventSubject::Window(window) = subject else {
                    panic!("destruction subject is a window");
                };
                (window, event)
            })
            .collect::<Vec<_>>();
        assert_eq!(allowed_sites, expected_sites);
        let expected_denied_sites = if opcode == 4 {
            vec![
                (DESTROY_PARENT, 18),
                (DESTROY_GRANDCHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_PARENT, 17),
                (DESTROY_PARENT, 17),
            ]
        } else {
            vec![
                (DESTROY_CHILD, 18),
                (DESTROY_PARENT, 18),
                (DESTROY_GRANDCHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_CHILD, 17),
                (DESTROY_PARENT, 17),
            ]
        };
        let denied_sites = received_sites(&receives, NOTIFY_RECEIVER_DENIED)
            .into_iter()
            .map(|(subject, event)| {
                let EventSubject::Window(window) = subject else {
                    panic!("destruction subject is a window");
                };
                (window, event)
            })
            .collect::<Vec<_>>();
        assert_eq!(denied_sites, expected_denied_sites);
        assert!(receives.iter().all(|call| {
            call.events.as_ref().is_some_and(|events| {
                events.len() == 1
                    && (events[0] == EventDescription::core(17)
                        || events[0] == EventDescription::core(18))
            })
        }));
    }
}

fn install_disconnect_policy(
    fixture: &mut NotifyFixture,
    deny: bool,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    fixture.install_policy(move |_, call| {
        if deny && call.hook == ExtensionHook::Receive && call.client == NOTIFY_RECEIVER_DENIED {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    })
}

// Mutation: skip Receive when execute_destroy runs from server teardown.
#[test]
fn receive_disconnect_destroy_notifies_survivors() {
    let mut baseline = destroy_fixture();
    assert!(!baseline.state.policies_installed());
    process_disconnect(&mut baseline.state, &mut baseline.backend, NOTIFY_SENDER);
    let baseline_denied_stream = baseline.drain(NOTIFY_RECEIVER_DENIED);
    let baseline_allowed_stream = baseline.drain(NOTIFY_RECEIVER_ALLOWED);

    let mut control = destroy_fixture();
    install_disconnect_policy(&mut control, false);
    process_disconnect(&mut control.state, &mut control.backend, NOTIFY_SENDER);
    let control_denied_stream = control.drain(NOTIFY_RECEIVER_DENIED);
    let control_allowed_stream = control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(control_denied_stream, baseline_denied_stream);
    assert_eq!(control_allowed_stream, baseline_allowed_stream);
    let control_backend = control.backend.calls();
    assert_destroy_cleanup(&control, 4);

    let mut denied = destroy_fixture();
    let calls = install_disconnect_policy(&mut denied, true);
    process_disconnect(&mut denied.state, &mut denied.backend, NOTIFY_SENDER);
    assert!(!denied.state.clients.contains_key(&NOTIFY_SENDER.0));
    assert!(denied.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        control_allowed_stream
    );
    assert_eq!(denied.backend.calls(), control_backend);
    assert_eq!(denied.state.selections, control.state.selections);
    assert_eq!(
        denied.state.clients[&NOTIFY_RECEIVER_DENIED.0].event_masks,
        control.state.clients[&NOTIFY_RECEIVER_DENIED.0].event_masks
    );
    assert_eq!(
        denied.state.clients[&NOTIFY_RECEIVER_ALLOWED.0].event_masks,
        control.state.clients[&NOTIFY_RECEIVER_ALLOWED.0].event_masks
    );
    assert_destroy_cleanup(&denied, 4);
    assert!(
        !control_denied_stream.is_empty(),
        "the allow control has selected events"
    );
    assert_eq!(
        event_tuples(&control_allowed_stream),
        vec![
            (18, DESTROY_PARENT.0, DESTROY_PARENT.0),
            (18, ROOT_WINDOW.0, DESTROY_PARENT.0),
            (17, DESTROY_GRANDCHILD.0, DESTROY_GRANDCHILD.0),
            (17, DESTROY_CHILD.0, DESTROY_GRANDCHILD.0),
            (17, DESTROY_CHILD.0, DESTROY_CHILD.0),
            (17, DESTROY_PARENT.0, DESTROY_CHILD.0),
            (17, DESTROY_PARENT.0, DESTROY_PARENT.0),
            (17, ROOT_WINDOW.0, DESTROY_PARENT.0),
        ]
    );

    let calls = calls.lock().unwrap();
    let receives = receives(&calls);
    assert!(!receives.is_empty(), "server teardown is checked");
    assert!(receives.iter().all(|call| call.request.is_none()));
    assert!(receives.iter().all(|call| call.client != NOTIFY_SENDER));
    assert!(
        receives
            .iter()
            .any(|call| call.client == NOTIFY_RECEIVER_DENIED)
    );
    assert!(
        receives
            .iter()
            .any(|call| call.client == NOTIFY_RECEIVER_ALLOWED)
    );
}

fn dispatch_composite(fixture: &mut NotifyFixture, client: ClientId, sequence: u16, minor: u8) {
    let body = ROOT_WINDOW.0.to_le_bytes();
    assert!(matches!(
        fixture
            .request(
                client,
                SequenceNumber(sequence),
                RequestHeader {
                    opcode: 144,
                    data: minor,
                    length_units: 2,
                },
                &body,
            )
            .expect("COMPOSITE request through process_request"),
        RequestOutcome::Handled
    ));
}

fn select_cow_notifications(fixture: &mut NotifyFixture) {
    for client in [NOTIFY_SENDER, NOTIFY_RECEIVER_ALLOWED] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("COW notification client")
            .event_masks
            .insert(ROOT_WINDOW, SUBSTRUCTURE_NOTIFY);
    }
}

type CowSnapshot = (
    Vec<ClientId>,
    Option<(ResourceId, MapState, Option<u32>)>,
    bool,
);

fn cow_snapshot(fixture: &NotifyFixture) -> CowSnapshot {
    let cow = fixture
        .state
        .resources
        .window(COMPOSITE_OVERLAY_WINDOW)
        .map(|window| {
            (
                window.parent,
                window.map_state,
                window.host_xid.map(WindowHandle::as_raw),
            )
        });
    (
        fixture.state.cow_claims.clone(),
        cow,
        fixture.backend.cow_materialized,
    )
}

fn reply_packet(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(bytes.len() % 32, 0, "whole reply/event packets");
    let replies = bytes
        .chunks_exact(32)
        .filter(|packet| packet[0] == 1)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1, "one GetOverlayWindow reply");
    replies[0].to_vec()
}

fn cow_events(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(bytes.len() % 32, 0, "whole event packets");
    bytes
        .chunks_exact(32)
        .filter(|packet| packet[0] != 1 && packet[0] != 0)
        .flatten()
        .copied()
        .collect()
}

fn install_cow_policy(fixture: &mut NotifyFixture) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    fixture.install_policy(|_, call| {
        if call.hook == ExtensionHook::Receive && call.client == NOTIFY_SENDER {
            Decision::Deny(DENY)
        } else {
            Decision::Allow
        }
    })
}

// Mutations: lose the extension minor; gate the reply/claim with Receive;
// exempt core events solely because the producer is COMPOSITE.
#[test]
fn receive_cow_notifications_keep_extension_request() {
    let mut baseline = NotifyFixture::new();
    select_cow_notifications(&mut baseline);
    assert!(!baseline.state.policies_installed());
    dispatch_composite(&mut baseline, NOTIFY_SENDER, 70, 7);
    let baseline_requestor_get = baseline.drain(NOTIFY_SENDER);
    let baseline_create_events = baseline.drain(NOTIFY_RECEIVER_ALLOWED);

    let mut control = NotifyFixture::new();
    select_cow_notifications(&mut control);
    control.install_policy(|_, _| Decision::Allow);
    dispatch_composite(&mut control, NOTIFY_SENDER, 70, 7);
    let control_requestor_get = control.drain(NOTIFY_SENDER);
    let control_create_events = control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(control_requestor_get, baseline_requestor_get);
    assert_eq!(control_create_events, baseline_create_events);
    assert_eq!(
        event_tuples(&control_create_events),
        vec![
            (16, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (19, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
        ]
    );
    let control_cow = cow_snapshot(&control);
    assert_eq!(control_cow.0, vec![NOTIFY_SENDER]);

    let mut denied = NotifyFixture::new();
    select_cow_notifications(&mut denied);
    let calls = install_cow_policy(&mut denied);
    dispatch_composite(&mut denied, NOTIFY_SENDER, 70, 7);
    let denied_requestor_get = denied.drain(NOTIFY_SENDER);
    let allowed_create_events = denied.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(cow_events(&denied_requestor_get), Vec::<u8>::new());
    assert_eq!(
        reply_packet(&denied_requestor_get),
        reply_packet(&control_requestor_get)
    );
    assert_eq!(
        cow_events(&allowed_create_events),
        cow_events(&control_create_events)
    );
    assert_eq!(cow_snapshot(&denied), control_cow);
    assert!(denied.state.clients.contains_key(&NOTIFY_SENDER.0));
    assert!(!denied.state.clients[&NOTIFY_SENDER.0].gone);
    assert!(!denied.state.clients[&NOTIFY_SENDER.0].write_failed);
    assert_eq!(denied.state.cow_claims, vec![NOTIFY_SENDER]);
    assert!(
        denied
            .state
            .resources
            .window(COMPOSITE_OVERLAY_WINDOW)
            .is_some()
    );

    for client in [NOTIFY_SENDER, NOTIFY_RECEIVER_ALLOWED] {
        denied
            .state
            .clients
            .get_mut(&client.0)
            .expect("live COW subscriber")
            .event_masks
            .insert(COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY);
        control
            .state
            .clients
            .get_mut(&client.0)
            .expect("live COW subscriber")
            .event_masks
            .insert(COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY);
    }

    dispatch_composite(&mut control, NOTIFY_SENDER, 71, 8);
    let control_release_requestor = control.drain(NOTIFY_SENDER);
    let control_release_allowed = control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(
        event_tuples(&control_release_requestor),
        vec![
            (18, COMPOSITE_OVERLAY_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (18, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (17, COMPOSITE_OVERLAY_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (17, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
        ]
    );
    assert_eq!(
        event_tuples(&control_release_allowed),
        vec![
            (18, COMPOSITE_OVERLAY_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (18, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (17, COMPOSITE_OVERLAY_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (17, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
        ]
    );
    assert_eq!(cow_snapshot(&control), (Vec::new(), None, false));

    dispatch_composite(&mut denied, NOTIFY_SENDER, 71, 8);
    assert!(denied.drain(NOTIFY_SENDER).is_empty());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        control_release_allowed
    );
    assert_eq!(cow_snapshot(&denied), cow_snapshot(&control));
    assert_eq!(denied.backend.calls(), control.backend.calls());

    let calls = calls.lock().unwrap();
    let receives = receives(&calls);
    for (minor, event_codes, subjects) in [
        (
            7,
            [16, 19].as_slice(),
            [ROOT_WINDOW, ROOT_WINDOW].as_slice(),
        ),
        (
            8,
            [18, 18, 17, 17].as_slice(),
            [
                COMPOSITE_OVERLAY_WINDOW,
                ROOT_WINDOW,
                COMPOSITE_OVERLAY_WINDOW,
                ROOT_WINDOW,
            ]
            .as_slice(),
        ),
    ] {
        let group = receives
            .iter()
            .copied()
            .filter(|call| call.request == request_opcodes(144, minor))
            .collect::<Vec<_>>();
        assert_eq!(group.len(), event_codes.len() * 2);
        let allowed_sites = received_sites(&group, NOTIFY_RECEIVER_ALLOWED)
            .into_iter()
            .map(|(subject, event)| {
                let EventSubject::Window(window) = subject else {
                    panic!("COW subject is a window");
                };
                (window, event)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            allowed_sites,
            subjects
                .iter()
                .copied()
                .zip(event_codes.iter().copied())
                .collect::<Vec<_>>()
        );
        for client in [NOTIFY_SENDER, NOTIFY_RECEIVER_ALLOWED] {
            let sites = received_sites(&group, client)
                .into_iter()
                .map(|(subject, event)| {
                    let EventSubject::Window(window) = subject else {
                        panic!("COW subject is a window");
                    };
                    (window, event)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                sites,
                subjects
                    .iter()
                    .copied()
                    .zip(event_codes.iter().copied())
                    .collect::<Vec<_>>()
            );
        }
        assert!(group.iter().all(|call| {
            call.events.as_ref().is_some_and(|events| {
                events.len() == 1
                    && [16, 17, 18, 19].contains(&events[0].type_code)
                    && events[0].extension.is_none()
            })
        }));
    }
}

// Mutation: reuse the previous COMPOSITE request context for COW server teardown.
#[test]
fn receive_last_cow_claim_disconnect_uses_no_request() {
    let mut baseline = NotifyFixture::new();
    select_cow_notifications(&mut baseline);
    assert!(!baseline.state.policies_installed());
    dispatch_composite(&mut baseline, NOTIFY_SENDER, 72, 7);
    let _ = baseline.drain(NOTIFY_SENDER);
    let _ = baseline.drain(NOTIFY_RECEIVER_ALLOWED);
    for client in [NOTIFY_SENDER, NOTIFY_RECEIVER_ALLOWED] {
        baseline
            .state
            .clients
            .get_mut(&client.0)
            .expect("live COW subscriber")
            .event_masks
            .insert(COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY);
    }
    process_disconnect(&mut baseline.state, &mut baseline.backend, NOTIFY_SENDER);
    let baseline_release_allowed = baseline.drain(NOTIFY_RECEIVER_ALLOWED);

    let mut control = NotifyFixture::new();
    select_cow_notifications(&mut control);
    control.install_policy(|_, _| Decision::Allow);
    dispatch_composite(&mut control, NOTIFY_SENDER, 72, 7);
    let _ = control.drain(NOTIFY_SENDER);
    let _ = control.drain(NOTIFY_RECEIVER_ALLOWED);
    for client in [NOTIFY_SENDER, NOTIFY_RECEIVER_ALLOWED] {
        control
            .state
            .clients
            .get_mut(&client.0)
            .expect("live COW subscriber")
            .event_masks
            .insert(COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY);
    }
    process_disconnect(&mut control.state, &mut control.backend, NOTIFY_SENDER);
    let control_release_allowed = control.drain(NOTIFY_RECEIVER_ALLOWED);
    assert_eq!(control_release_allowed, baseline_release_allowed);
    assert_eq!(
        event_tuples(&control_release_allowed),
        vec![
            (18, COMPOSITE_OVERLAY_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (18, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (17, COMPOSITE_OVERLAY_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
            (17, ROOT_WINDOW.0, COMPOSITE_OVERLAY_WINDOW.0),
        ]
    );

    let mut denied = NotifyFixture::new();
    select_cow_notifications(&mut denied);
    let calls = install_cow_policy(&mut denied);
    dispatch_composite(&mut denied, NOTIFY_SENDER, 72, 7);
    let _ = denied.drain(NOTIFY_SENDER);
    let _ = denied.drain(NOTIFY_RECEIVER_ALLOWED);
    for client in [NOTIFY_SENDER, NOTIFY_RECEIVER_ALLOWED] {
        denied
            .state
            .clients
            .get_mut(&client.0)
            .expect("live COW subscriber")
            .event_masks
            .insert(COMPOSITE_OVERLAY_WINDOW, STRUCTURE_NOTIFY);
    }
    calls.lock().unwrap().clear();
    process_disconnect(&mut denied.state, &mut denied.backend, NOTIFY_SENDER);
    assert!(!denied.state.clients.contains_key(&NOTIFY_SENDER.0));
    assert!(denied.drain(NOTIFY_RECEIVER_DENIED).is_empty());
    assert_eq!(
        denied.drain(NOTIFY_RECEIVER_ALLOWED),
        control_release_allowed
    );
    assert_eq!(cow_snapshot(&denied), (Vec::new(), None, false));
    assert_eq!(denied.backend.calls(), control.backend.calls());

    let calls = calls.lock().unwrap();
    let receives = receives(&calls);
    assert_eq!(
        received_sites(&receives, NOTIFY_RECEIVER_ALLOWED),
        vec![
            (EventSubject::Window(COMPOSITE_OVERLAY_WINDOW), 18),
            (EventSubject::Window(ROOT_WINDOW), 18),
            (EventSubject::Window(COMPOSITE_OVERLAY_WINDOW), 17),
            (EventSubject::Window(ROOT_WINDOW), 17),
        ]
    );
    assert!(receives.iter().all(|call| call.request.is_none()));
    assert!(receives.iter().all(|call| call.client != NOTIFY_SENDER));
}

// Keep existing fixture tests private while letting the Task 8 parity test
// rerun these established no-policy/Allow comparisons from its sibling module.
pub(super) fn run_no_policy_parity_fixtures() {
    receive_destroy_denial_keeps_cleanup();
    receive_disconnect_destroy_notifies_survivors();
    receive_cow_notifications_keep_extension_request();
    receive_last_cow_claim_disconnect_uses_no_request();
}
