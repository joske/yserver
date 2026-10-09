use super::*;

fn sync_header(minor: u8) -> RequestHeader {
    RequestHeader {
        opcode: 142,
        data: minor,
        length_units: 0,
    }
}

fn counter_value_body(counter: u32, value: i64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&counter.to_le_bytes());
    b.extend_from_slice(&xsync_i64(value));
    b
}

// The exact CreateAlarm muffin issues under Cinnamon: Relative
// value 1, PositiveComparison, delta 1, events true, watching
// counter 0x02600006.
fn muffin_alarm_body() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x01e0_0019u32.to_le_bytes());
    let mask = x11sync::CA_COUNTER
        | x11sync::CA_VALUE_TYPE
        | x11sync::CA_VALUE
        | x11sync::CA_TEST_TYPE
        | x11sync::CA_DELTA
        | x11sync::CA_EVENTS;
    b.extend_from_slice(&mask.to_le_bytes());
    b.extend_from_slice(&0x0260_0006u32.to_le_bytes());
    b.extend_from_slice(&x11sync::VALUE_TYPE_RELATIVE.to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes()); // value hi
    b.extend_from_slice(&1u32.to_le_bytes()); // value lo
    b.extend_from_slice(&x11sync::TEST_POSITIVE_COMPARISON.to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes()); // delta hi
    b.extend_from_slice(&1u32.to_le_bytes()); // delta lo
    b.push(1); // events = true
    b.extend_from_slice(&[0u8; 3]);
    b
}

// Reproduces the Cinnamon frame-sync deadlock: muffin arms a SYNC
// alarm on a GTK client's _NET_WM_SYNC_REQUEST_COUNTER and depends
// on AlarmNotify to learn the client finished a frame. Before the
// fix, SetCounter never evaluated alarms and no AlarmNotify was
// ever sent, so muffin never composited and clients froze.
#[test]
fn sync_alarm_fires_alarmnotify_on_watched_counter_crossing() {
    let mut state = ServerState::new();
    let mut muffin = install_client(&mut state, 1);
    let mut gtk = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    // GTK client (2) creates its frame-sync counter at 0.
    handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(1),
        sync_header(2), // CREATE_COUNTER
        &counter_value_body(0x0260_0006, 0),
    )
    .unwrap();

    // muffin (1) arms the alarm. wait_value = counter(0) + 1 = 1,
    // so the create-time test (0 >= 1) does not fire.
    handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        sync_header(8), // CREATE_ALARM
        &muffin_alarm_body(),
    )
    .unwrap();
    assert!(
        read_all_available(&mut muffin).is_empty(),
        "alarm must not fire at creation when wait_value exceeds the counter"
    );

    // GTK client finishes a frame: counter 0 -> 2, crossing 1.
    handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(2),
        sync_header(3), // SET_COUNTER
        &counter_value_body(0x0260_0006, 2),
    )
    .unwrap();

    let evt = read_all_available(&mut muffin);
    assert_eq!(evt.len(), 32, "exactly one AlarmNotify to the alarm owner");
    assert_eq!(evt[0], 84, "type = SYNC first-event(83) + AlarmNotify(1)");
    assert_eq!(evt[1], 1, "kind = AlarmNotify");
    assert_eq!(
        u32::from_le_bytes(evt[4..8].try_into().unwrap()),
        0x01e0_0019,
        "alarm id"
    );
    assert_eq!(
        u32::from_le_bytes(evt[12..16].try_into().unwrap()),
        2,
        "counter value carried on the event"
    );
    assert_eq!(
        u32::from_le_bytes(evt[20..24].try_into().unwrap()),
        1,
        "alarm (wait) value that triggered"
    );
    assert_eq!(evt[28], 0, "non-zero delta re-arms -> state Active");
    assert!(
        read_all_available(&mut gtk).is_empty(),
        "AlarmNotify goes to the alarm owner, not the counter setter"
    );

    // Re-arm proof: the next frame (2 -> 4) must fire again. Without
    // re-arm the muffin frame loop would stall after one frame.
    handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(3),
        sync_header(3),
        &counter_value_body(0x0260_0006, 4),
    )
    .unwrap();
    let evt2 = read_all_available(&mut muffin);
    assert_eq!(
        evt2.len(),
        32,
        "second crossing fires again (alarm re-armed)"
    );
    assert_eq!(
        u32::from_le_bytes(evt2[12..16].try_into().unwrap()),
        4,
        "second event carries the new counter value"
    );
}

#[test]
fn sync_create_trigger_query_fence_round_trip() {
    let mut state = ServerState::new();
    let peer_a = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();

    // Client A: CreateFence(fence=0x42, initially_triggered=false).
    let mut create_body = vec![0u8; 12];
    create_body[0..4].copy_from_slice(&0u32.to_le_bytes()); // drawable=0
    create_body[4..8].copy_from_slice(&0x42u32.to_le_bytes());
    create_body[8] = 0;
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 142,
            data: 14, // CREATE_FENCE
            length_units: 4,
        },
        &create_body,
        None,
    )
    .unwrap();

    // Client B: QueryFence -> triggered=false initially.
    let mut q_body = vec![0u8; 4];
    q_body[0..4].copy_from_slice(&0x42u32.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(1),
        RequestHeader {
            opcode: 142,
            data: 18, // QUERY_FENCE
            length_units: 2,
        },
        &q_body,
        None,
    )
    .unwrap();
    peer_b.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer_b.read_exact(&mut buf).unwrap();
    assert_eq!(buf[0], 1); // Reply
    assert_eq!(buf[8], 0, "fence not yet triggered");

    // Client A: TriggerFence.
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: 142,
            data: 15, // TRIGGER_FENCE
            length_units: 2,
        },
        &q_body,
        None,
    )
    .unwrap();

    // Client B: QueryFence -> triggered=true now.
    process_request(
        &mut state,
        &mut backend,
        ClientId(2),
        SequenceNumber(2),
        RequestHeader {
            opcode: 142,
            data: 18, // QUERY_FENCE
            length_units: 2,
        },
        &q_body,
        None,
    )
    .unwrap();
    let mut buf2 = [0u8; 32];
    peer_b.read_exact(&mut buf2).unwrap();
    assert_eq!(buf2[8], 1, "fence triggered after Client A's trigger");

    // Stash peer_a so it doesn't drop and close the writer mid-test.
    let _ = peer_a;
}

// ── SYNC Await / AwaitFence ─────────────────────────────────────
// Ground truth for every expectation below: Xorg 21.1.24 Xvfb driven
// by a two-connection xcb probe (client A changes counters and fences,
// client B awaits and then sends GetInputFocus; "suspended" means B's
// reply had not arrived after A's round trip).

fn sync_req(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    minor: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(1),
        RequestHeader {
            opcode: 142,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("sync request");
}

fn sync_i64(v: i64) -> [u8; 8] {
    let mut out = [0u8; 8];
    #[allow(clippy::cast_possible_truncation)]
    out[..4].copy_from_slice(&((v >> 32) as i32).to_le_bytes());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    out[4..].copy_from_slice(&(v as u32).to_le_bytes());
    out
}

fn sync_counter_body(counter: u32, value: i64) -> Vec<u8> {
    let mut body = counter.to_le_bytes().to_vec();
    body.extend_from_slice(&sync_i64(value));
    body
}

/// One `WAITCONDITION`: counter, value type, wait value, test type,
/// event threshold.
fn sync_wait(counter: u32, value_type: u32, wait: i64, test_type: u32, threshold: i64) -> Vec<u8> {
    let mut body = counter.to_le_bytes().to_vec();
    body.extend_from_slice(&value_type.to_le_bytes());
    body.extend_from_slice(&sync_i64(wait));
    body.extend_from_slice(&test_type.to_le_bytes());
    body.extend_from_slice(&sync_i64(threshold));
    body
}

/// CounterNotify → (counter, wait value, counter value, count, destroyed).
fn counter_notify_fields(p: &[u8; 32]) -> (u32, i64, i64, u16, bool) {
    assert_eq!(p[0], crate::nested::SYNC_FIRST_EVENT, "CounterNotify type");
    assert_eq!(p[1], 0, "kind CounterNotify");
    let i64_at = |at: usize| {
        #[allow(clippy::cast_possible_wrap)]
        let hi = le_u32(p, at) as i32;
        (i64::from(hi) << 32) | i64::from(le_u32(p, at + 4))
    };
    (
        le_u32(p, 4),
        i64_at(8),
        i64_at(16),
        u16::from_le_bytes([p[28], p[29]]),
        p[30] != 0,
    )
}

struct SyncFixture {
    state: ServerState,
    backend: RecordingBackend,
    _peer_a: UnixStream,
    peer_b: UnixStream,
}

const SYNC_A: u32 = 1;
const SYNC_B: u32 = 2;
const SYNC_C1: u32 = 0x0010_0001;
const SYNC_C2: u32 = 0x0010_0002;

fn sync_fixture() -> SyncFixture {
    let mut state = ServerState::new();
    let peer_a = install_client(&mut state, SYNC_A);
    let peer_b = install_client(&mut state, SYNC_B);
    SyncFixture {
        state,
        backend: RecordingBackend::new(),
        _peer_a: peer_a,
        peer_b,
    }
}

impl SyncFixture {
    fn a(&mut self, minor: u8, body: &[u8]) {
        sync_req(&mut self.state, &mut self.backend, SYNC_A, minor, body);
    }
    fn b(&mut self, minor: u8, body: &[u8]) {
        sync_req(&mut self.state, &mut self.backend, SYNC_B, minor, body);
    }
    fn suspended(&self) -> bool {
        crate::core_loop::sync_await::client_is_suspended(&self.state, ClientId(SYNC_B))
    }
    fn b_packets(&mut self) -> Vec<[u8; 32]> {
        wire_packets(&mut self.peer_b)
    }
    fn a_packets(&mut self) -> Vec<[u8; 32]> {
        wire_packets(&mut self._peer_a)
    }
}

/// Xvfb: B awaits c1 >= 5 (PositiveComparison); A sets 3 → B stays
/// suspended; A sets 7 → B resumes with CounterNotify(c1, wait 5,
/// value 7, count 0, not destroyed).
#[test]
fn sync_await_suspends_until_the_counter_condition_holds() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 0));
    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            5,
            s::TEST_POSITIVE_COMPARISON,
            0,
        ),
    );
    assert!(f.suspended());
    assert!(f.b_packets().is_empty());
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 3));
    assert!(f.suspended(), "3 < 5");
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 7));
    assert!(!f.suspended());
    let events = f.b_packets();
    assert_eq!(events.len(), 1);
    assert_eq!(counter_notify_fields(&events[0]), (SYNC_C1, 5, 7, 0, false));

    // Already satisfied at request time: no suspension, the event
    // still goes out (Xvfb: reply arrives with CounterNotify wait 5
    // value 7).
    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            5,
            s::TEST_POSITIVE_COMPARISON,
            0,
        ),
    );
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (SYNC_C1, 5, 7, 0, false)
    );
}

/// Transitions need a crossing and events respect the threshold, per
/// Xvfb: PositiveTransition 10 thr 2 from 7 → set 9 (no), set 12 →
/// event value 12; from 12, PositiveTransition 10 → set 1 does not
/// fire (no upward crossing); from 1, PositiveTransition 10 → 5 (no),
/// 11 → event; PositiveTransition 0 thr 5 from -5 → set 1 resumes the
/// client without an event (diff 1 < 5).
#[test]
fn sync_await_transitions_and_event_threshold() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 7));
    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            10,
            s::TEST_POSITIVE_TRANSITION,
            2,
        ),
    );
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 9));
    assert!(f.suspended());
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 12));
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (SYNC_C1, 10, 12, 0, false)
    );

    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            10,
            s::TEST_POSITIVE_TRANSITION,
            100,
        ),
    );
    assert!(f.suspended(), "12 >= 10 but no transition yet");
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 1));
    assert!(f.suspended());
    f.state.sync_awaits.remove(&SYNC_B);

    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            10,
            s::TEST_POSITIVE_TRANSITION,
            0,
        ),
    );
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 5));
    assert!(f.suspended());
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 11));
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (SYNC_C1, 10, 11, 0, false)
    );

    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, -5));
    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            0,
            s::TEST_POSITIVE_TRANSITION,
            5,
        ),
    );
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 1));
    assert!(!f.suspended());
    assert!(f.b_packets().is_empty(), "diff below threshold: no event");
}

/// Xvfb: c1=11, c2=100; B awaits [c1 >= 1000 thr -2000, c2 <= 45 thr
/// 0]; A sets c2 50 (suspended) then 40 → both conditions report, in
/// order, count 1 then 0. Relative waits add to the counter (c1=11,
/// +3 → wait 14); NegativeTransition -3 fires on the way down.
#[test]
fn sync_await_multi_condition_relative_and_negative() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 11));
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C2, 100));
    let mut body = sync_wait(
        SYNC_C1,
        s::VALUE_TYPE_ABSOLUTE,
        1000,
        s::TEST_POSITIVE_COMPARISON,
        -2000,
    );
    body.extend(sync_wait(
        SYNC_C2,
        s::VALUE_TYPE_ABSOLUTE,
        45,
        s::TEST_NEGATIVE_COMPARISON,
        0,
    ));
    f.b(s::AWAIT, &body);
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C2, 50));
    assert!(f.suspended());
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C2, 40));
    assert!(!f.suspended());
    let events = f.b_packets();
    assert_eq!(events.len(), 2);
    assert_eq!(
        counter_notify_fields(&events[0]),
        (SYNC_C1, 1000, 11, 1, false)
    );
    assert_eq!(
        counter_notify_fields(&events[1]),
        (SYNC_C2, 45, 40, 0, false)
    );

    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_RELATIVE,
            3,
            s::TEST_POSITIVE_COMPARISON,
            0,
        ),
    );
    f.a(s::CHANGE_COUNTER, &sync_counter_body(SYNC_C1, 2));
    assert!(f.suspended());
    f.a(s::CHANGE_COUNTER, &sync_counter_body(SYNC_C1, 1));
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (SYNC_C1, 14, 14, 0, false)
    );

    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            -3,
            s::TEST_NEGATIVE_TRANSITION,
            0,
        ),
    );
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, -5));
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (SYNC_C1, -3, -5, 0, false)
    );
}

/// Xvfb: destroying an awaited counter resumes the client with a
/// destroyed event carrying its last value, plus threshold events for
/// the other conditions; an alarm on it goes Inactive with an
/// AlarmNotify. A counter owner disconnecting does the same (value 0).
#[test]
fn sync_await_fires_destroyed_when_the_counter_goes_away() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, -5));
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C2, 40));
    let mut body = sync_wait(
        SYNC_C1,
        s::VALUE_TYPE_ABSOLUTE,
        1000,
        s::TEST_POSITIVE_COMPARISON,
        0,
    );
    body.extend(sync_wait(
        SYNC_C2,
        s::VALUE_TYPE_ABSOLUTE,
        1000,
        s::TEST_POSITIVE_COMPARISON,
        -2000,
    ));
    f.b(s::AWAIT, &body);
    f.state.sync_alarms.insert(
        0x0020_0009,
        crate::server::SyncAlarm {
            owner: ClientId(SYNC_B),
            counter: SYNC_C1,
            wait_value: 1000,
            delta: 1,
            test_type: s::TEST_POSITIVE_COMPARISON,
            events: true,
            state: s::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 1000,
            check_type: s::TEST_POSITIVE_COMPARISON,
        },
    );
    f.a(s::DESTROY_COUNTER, &SYNC_C1.to_le_bytes());
    assert!(!f.suspended());
    let packets = f.b_packets();
    let notifies: Vec<_> = packets
        .iter()
        .filter(|p| p[0] == crate::nested::SYNC_FIRST_EVENT)
        .map(counter_notify_fields)
        .collect();
    assert_eq!(
        notifies,
        vec![(SYNC_C1, 1000, -5, 1, true), (SYNC_C2, 1000, 40, 0, false)]
    );
    let alarm = packets
        .iter()
        .find(|p| p[0] == crate::nested::SYNC_FIRST_EVENT + 1)
        .expect("AlarmNotify for the alarm on the destroyed counter");
    assert_eq!(alarm[28], s::ALARM_STATE_INACTIVE);
    assert_eq!(
        f.state.sync_alarms[&0x0020_0009].state,
        s::ALARM_STATE_INACTIVE
    );
    assert_eq!(f.state.sync_alarms[&0x0020_0009].counter, 0);

    // Owner disconnect: Xvfb sends destroyed, wait 10, value 0.
    const C3: u32 = 0x0030_0001;
    let _peer_c = install_client(&mut f.state, 3);
    sync_req(
        &mut f.state,
        &mut f.backend,
        3,
        s::CREATE_COUNTER,
        &sync_counter_body(C3, 0),
    );
    f.b(
        s::AWAIT,
        &sync_wait(
            C3,
            s::VALUE_TYPE_ABSOLUTE,
            10,
            s::TEST_POSITIVE_COMPARISON,
            0,
        ),
    );
    assert!(f.suspended());
    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(3),
    );
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (C3, 10, 0, 0, true)
    );
}

/// Xvfb: Initialize with client 3.1 / 3.0 / 2.0 / 4.0 always replies 3.1.
#[test]
fn sync_initialize_always_answers_the_server_version() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    for (major, minor) in [(3u8, 1u8), (3, 0), (2, 0), (4, 0)] {
        f.b(s::INITIALIZE, &[major, minor, 0, 0]);
        let reply = f.b_packets();
        assert_eq!(reply[0][0], 1);
        assert_eq!((reply[0][8], reply[0][9]), (3, 1), "client {major}.{minor}");
    }
}

/// Xvfb error table for Await and the counter requests.
#[test]
fn sync_await_and_counter_errors_match_xvfb() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    let bad_counter = crate::nested::SYNC_FIRST_ERROR + s::BAD_COUNTER;
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C2, 100));
    let cases: [(Vec<u8>, u8, u32); 6] = [
        (Vec::new(), x11::error::BAD_VALUE, 0),
        (sync_wait(0, 0, 1, 2, 0), bad_counter, 0),
        (sync_wait(0x0bad_bad0, 0, 1, 2, 0), bad_counter, 0x0bad_bad0),
        (sync_wait(SYNC_C2, 7, 1, 2, 0), x11::error::BAD_VALUE, 7),
        (sync_wait(SYNC_C2, 0, 1, 9, 0), x11::error::BAD_VALUE, 9),
        (
            sync_wait(SYNC_C2, 1, i64::MAX, 2, 0),
            x11::error::BAD_VALUE,
            0x7fff_ffff,
        ),
    ];
    for (body, code, value) in cases {
        f.b(s::AWAIT, &body);
        assert!(!f.suspended(), "an erroring Await never suspends");
        assert_eq!(
            error_fields(&f.b_packets()[0]),
            (code, value, u16::from(s::AWAIT), 142)
        );
    }
    let mut peer_a = std::mem::replace(&mut f._peer_a, UnixStream::pair().unwrap().0);
    let st = s::SERVERTIME_COUNTER;
    let a_cases: [(u8, Vec<u8>, u8, u32); 5] = [
        (
            s::SET_COUNTER,
            sync_counter_body(st, 1),
            x11::error::BAD_ACCESS,
            st,
        ),
        (
            s::SET_COUNTER,
            sync_counter_body(0x0bad_bad0, 1),
            bad_counter,
            0x0bad_bad0,
        ),
        (
            s::CHANGE_COUNTER,
            sync_counter_body(SYNC_C2, i64::MAX),
            x11::error::BAD_VALUE,
            0x7fff_ffff,
        ),
        (
            s::DESTROY_COUNTER,
            st.to_le_bytes().to_vec(),
            x11::error::BAD_ACCESS,
            st,
        ),
        (
            s::DESTROY_COUNTER,
            0x0bad_bad0u32.to_le_bytes().to_vec(),
            bad_counter,
            0x0bad_bad0,
        ),
    ];
    for (minor, body, code, value) in a_cases {
        f.a(minor, &body);
        assert_eq!(
            error_fields(&wire_packets(&mut peer_a)[0]),
            (code, value, u16::from(minor), 142)
        );
    }
    assert_eq!(
        f.state.sync_counters[&SYNC_C2].value, 100,
        "overflow left it alone"
    );
}

/// Xvfb: AwaitFence on an untriggered fence suspends until TriggerFence
/// (no events); on a triggered fence it returns at once; destroying the
/// fence resumes with CounterNotify(counter = fence, 0, 0, destroyed).
/// Errors: empty → BadValue, None / unknown → BadFence; ResetFence of an
/// untriggered fence → BadMatch naming it.
#[test]
fn sync_await_fence_suspends_until_triggered() {
    use yserver_protocol::x11::sync as s;
    const F1: u32 = 0x0010_0010;
    const F2: u32 = 0x0010_0011;
    let mut f = sync_fixture();
    let fence_body = |fence: u32, triggered: bool| {
        let mut body = ROOT_WINDOW.0.to_le_bytes().to_vec();
        body.extend_from_slice(&fence.to_le_bytes());
        body.extend_from_slice(&[u8::from(triggered), 0, 0, 0]);
        body
    };
    f.a(s::CREATE_FENCE, &fence_body(F1, false));
    f.a(s::CREATE_FENCE, &fence_body(F2, true));

    f.b(s::AWAIT_FENCE, &F1.to_le_bytes());
    assert!(f.suspended());
    f.a(s::TRIGGER_FENCE, &F1.to_le_bytes());
    assert!(!f.suspended());
    assert!(
        f.b_packets().is_empty(),
        "fences send no events when triggered"
    );

    f.b(s::AWAIT_FENCE, &F2.to_le_bytes());
    assert!(!f.suspended(), "already triggered");

    f.a(s::RESET_FENCE, &F1.to_le_bytes());
    f.b(s::AWAIT_FENCE, &F1.to_le_bytes());
    assert!(f.suspended());
    f.a(s::DESTROY_FENCE, &F1.to_le_bytes());
    assert!(!f.suspended());
    assert_eq!(
        counter_notify_fields(&f.b_packets()[0]),
        (F1, 0, 0, 0, true)
    );

    let bad_fence = crate::nested::SYNC_FIRST_ERROR + s::BAD_FENCE;
    for (body, code, value) in [
        (Vec::new(), x11::error::BAD_VALUE, 0),
        (0u32.to_le_bytes().to_vec(), bad_fence, 0),
        (
            0x0bad_bad0u32.to_le_bytes().to_vec(),
            bad_fence,
            0x0bad_bad0,
        ),
    ] {
        f.b(s::AWAIT_FENCE, &body);
        assert!(!f.suspended());
        assert_eq!(
            error_fields(&f.b_packets()[0]),
            (code, value, u16::from(s::AWAIT_FENCE), 142)
        );
    }
    f.b(s::RESET_FENCE, &F2.to_le_bytes());
    f.b(s::RESET_FENCE, &F2.to_le_bytes());
    assert_eq!(
        error_fields(&f.b_packets()[0]),
        (x11::error::BAD_MATCH, F2, u16::from(s::RESET_FENCE), 142)
    );
}

/// A Present idle fence triggered by the server wakes an AwaitFence on
/// it. A deliberate difference from Xorg: `present_fence_set_triggered`
/// only calls the fence's `SetTriggered`, not `miSyncTriggerFence`, so
/// on Xorg such an await is only re-checked by a TriggerFence request
/// or the fence's destruction and would otherwise stay suspended.
#[test]
fn sync_await_fence_wakes_on_server_side_trigger() {
    use yserver_protocol::x11::sync as s;
    const F1: u32 = 0x0010_0020;
    let mut f = sync_fixture();
    f.state.sync_fences.insert(
        F1,
        crate::server::SyncFence {
            owner: ClientId(SYNC_A),
            triggered: false,
        },
    );
    f.b(s::AWAIT_FENCE, &F1.to_le_bytes());
    assert!(f.suspended());
    crate::core_loop::sync_await::fence_triggered(&mut f.state, F1);
    assert!(!f.suspended());
    assert!(f.state.sync_fences[&F1].triggered);
}

/// A ChangeAlarm body: alarm, value mask, value list.
fn sync_alarm_body(alarm: u32, mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = alarm.to_le_bytes().to_vec();
    body.extend_from_slice(&mask.to_le_bytes());
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

/// Xvfb: ChangeAlarm / QueryAlarm / DestroyAlarm on an alarm that
/// does not exist (unknown, None, or destroyed) answer BadAlarm (SYNC
/// error base + 1) naming it. Xorg's order: QueryAlarm and DestroyAlarm
/// check the exact request size first (BadLength), ChangeAlarm checks
/// the minimum size, then looks the alarm up, and only then matches the
/// value list against the mask (BadLength naming the alarm).
#[test]
fn sync_alarm_requests_on_a_missing_alarm_answer_bad_alarm() {
    use yserver_protocol::x11::sync as s;
    const ALARM: u32 = 0x0010_0030;
    const GONE: u32 = 0x0010_0031;
    const UNKNOWN: u32 = 0x0bad_bad0;
    let bad_alarm = crate::nested::SYNC_FIRST_ERROR + s::BAD_ALARM;
    let bad_length = x11::error::BAD_LENGTH;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 0));
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(ALARM, s::CA_COUNTER, &[SYNC_C1]),
    );
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(GONE, s::CA_COUNTER, &[SYNC_C1]),
    );
    f.a(s::DESTROY_ALARM, &GONE.to_le_bytes());
    let _ = f.b_packets();
    let long = sync_alarm_body(UNKNOWN, s::CA_EVENTS, &[]);
    let cases: Vec<(u8, Vec<u8>, u8, u32)> = vec![
        (
            s::QUERY_ALARM,
            UNKNOWN.to_le_bytes().to_vec(),
            bad_alarm,
            UNKNOWN,
        ),
        (s::QUERY_ALARM, 0u32.to_le_bytes().to_vec(), bad_alarm, 0),
        (s::QUERY_ALARM, GONE.to_le_bytes().to_vec(), bad_alarm, GONE),
        (
            s::CHANGE_ALARM,
            sync_alarm_body(UNKNOWN, 0, &[]),
            bad_alarm,
            UNKNOWN,
        ),
        (
            s::CHANGE_ALARM,
            sync_alarm_body(GONE, 0, &[]),
            bad_alarm,
            GONE,
        ),
        (s::CHANGE_ALARM, long.clone(), bad_alarm, UNKNOWN),
        (
            s::DESTROY_ALARM,
            UNKNOWN.to_le_bytes().to_vec(),
            bad_alarm,
            UNKNOWN,
        ),
        (
            s::DESTROY_ALARM,
            GONE.to_le_bytes().to_vec(),
            bad_alarm,
            GONE,
        ),
        (s::QUERY_ALARM, long.clone(), bad_length, 0),
        (s::DESTROY_ALARM, long, bad_length, 0),
        (
            s::CHANGE_ALARM,
            UNKNOWN.to_le_bytes().to_vec(),
            bad_length,
            0,
        ),
        (
            s::CHANGE_ALARM,
            sync_alarm_body(ALARM, s::CA_EVENTS, &[]),
            bad_length,
            ALARM,
        ),
        (
            s::CHANGE_ALARM,
            sync_alarm_body(ALARM, s::CA_VALUE, &[0]),
            bad_length,
            ALARM,
        ),
    ];
    for (minor, body, code, value) in cases {
        f.b(minor, &body);
        let packets = f.b_packets();
        assert!(
            !packets.is_empty(),
            "minor {minor} body {body:x?}: no error"
        );
        assert_eq!(
            error_fields(&packets[0]),
            (code, value, u16::from(minor), 142),
            "minor {minor} body {body:x?}"
        );
    }
    assert!(f.state.sync_alarms.contains_key(&ALARM));
}

/// QueryAlarm → (counter, value type, wait value, test type, delta,
/// events, state), per `xSyncQueryAlarmReply`.
fn query_alarm_fields(f: &mut SyncFixture, alarm: u32) -> (u32, u32, i64, u32, i64, u8, u8) {
    use yserver_protocol::x11::sync as s;
    f.b(s::QUERY_ALARM, &alarm.to_le_bytes());
    let r = read_all_available(&mut f.peer_b);
    assert_eq!((r.len(), r[0]), (40, 1), "QueryAlarm reply");
    let i64_at = |at: usize| {
        #[allow(clippy::cast_possible_wrap)]
        let hi = le_u32(&r, at) as i32;
        (i64::from(hi) << 32) | i64::from(le_u32(&r, at + 4))
    };
    (
        le_u32(&r, 8),
        le_u32(&r, 12),
        i64_at(16),
        le_u32(&r, 24),
        i64_at(28),
        r[36],
        r[37],
    )
}

/// Xvfb: QueryAlarm reports the alarm's test type (a fresh alarm's
/// default is PositiveComparison, 2) and, as Xorg's
/// `ProcSyncQueryAlarm`, always value type Absolute with the resolved
/// wait value.
#[test]
fn sync_query_alarm_reports_the_test_type() {
    use yserver_protocol::x11::sync as s;
    const AL: u32 = 0x0010_0080;
    const AN: u32 = 0x0010_0081;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 0));
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(AL, s::CA_COUNTER | s::CA_VALUE, &[SYNC_C1, 0, 10]),
    );
    assert_eq!(
        query_alarm_fields(&mut f, AL),
        (
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            10,
            s::TEST_POSITIVE_COMPARISON,
            1,
            1,
            s::ALARM_STATE_ACTIVE
        )
    );
    f.a(s::CREATE_ALARM, &sync_alarm_body(AN, s::CA_VALUE, &[0, 5]));
    assert_eq!(
        query_alarm_fields(&mut f, AN),
        (
            0,
            s::VALUE_TYPE_ABSOLUTE,
            5,
            s::TEST_POSITIVE_COMPARISON,
            1,
            1,
            s::ALARM_STATE_INACTIVE
        )
    );
}

/// Xvfb ("CreateAlarm attribute errors"): Xorg's checks, in its order —
/// the value list in mask-bit order (`events` not True/False, unknown
/// bits naming the bits above), the delta sign against the test type
/// (the default delta 1 counts), then the trigger: unknown counter,
/// value type, Relative without a counter, INT64 overflow (naming the
/// value's high word), test type. BadMatch names the alarm. A failing
/// CreateAlarm creates nothing.
#[test]
fn sync_create_alarm_attribute_errors_match_xvfb() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 14));
    let bad_counter = crate::nested::SYNC_FIRST_ERROR + s::BAD_COUNTER;
    let (value, matchh) = (x11::error::BAD_VALUE, x11::error::BAD_MATCH);
    let c = SYNC_C1;
    let unknown = 0x0bad_bad0;
    // (mask, values, expected error (code, value; None = the alarm id)).
    type Case = (u32, Vec<u32>, Option<(u8, Option<u32>)>);
    let cases: Vec<Case> = vec![
        (
            s::CA_COUNTER | s::CA_TEST_TYPE,
            vec![c, 9],
            Some((value, Some(9))),
        ),
        (
            s::CA_COUNTER | s::CA_VALUE_TYPE,
            vec![c, 7],
            Some((value, Some(7))),
        ),
        (
            s::CA_COUNTER | s::CA_EVENTS,
            vec![c, 2],
            Some((value, Some(2))),
        ),
        (
            s::CA_COUNTER,
            vec![unknown],
            Some((bad_counter, Some(unknown))),
        ),
        (
            s::CA_VALUE_TYPE,
            vec![s::VALUE_TYPE_RELATIVE],
            Some((matchh, None)),
        ),
        (
            s::CA_COUNTER | s::CA_TEST_TYPE | s::CA_DELTA,
            vec![c, s::TEST_POSITIVE_COMPARISON, u32::MAX, u32::MAX],
            Some((matchh, None)),
        ),
        (
            s::CA_COUNTER | s::CA_TEST_TYPE,
            vec![c, s::TEST_NEGATIVE_COMPARISON],
            Some((matchh, None)),
        ),
        (
            s::CA_COUNTER | s::CA_TEST_TYPE | s::CA_DELTA,
            vec![c, s::TEST_NEGATIVE_COMPARISON, 0, 0],
            None,
        ),
        (
            s::CA_COUNTER | s::CA_VALUE_TYPE | s::CA_VALUE,
            vec![c, s::VALUE_TYPE_RELATIVE, 0x7fff_ffff, u32::MAX],
            Some((value, Some(0x7fff_ffff))),
        ),
        (
            s::CA_COUNTER | s::CA_TEST_TYPE,
            vec![unknown, 9],
            Some((bad_counter, Some(unknown))),
        ),
        (
            s::CA_VALUE_TYPE | s::CA_TEST_TYPE,
            vec![7, 9],
            Some((value, Some(7))),
        ),
        (1 << 6, vec![0], Some((value, Some(0)))),
        (s::CA_EVENTS | 1 << 6, vec![2, 0], Some((value, Some(2)))),
    ];
    for (index, (mask, values, expect)) in cases.into_iter().enumerate() {
        let alarm = 0x0020_0100 + u32::try_from(index).unwrap();
        f.b(s::CREATE_ALARM, &sync_alarm_body(alarm, mask, &values));
        let packets = f.b_packets();
        match expect {
            Some((code, bad)) => {
                assert_eq!(
                    error_fields(&packets[0]),
                    (code, bad.unwrap_or(alarm), u16::from(s::CREATE_ALARM), 142),
                    "case {index}"
                );
                assert!(!f.state.sync_alarms.contains_key(&alarm), "case {index}");
            }
            None => {
                assert!(packets.is_empty(), "case {index}");
                assert!(f.state.sync_alarms.contains_key(&alarm), "case {index}");
            }
        }
    }
}

/// Xvfb ("ChangeAlarm attribute errors: what sticks"): a failing
/// ChangeAlarm keeps what Xorg's `SyncChangeAlarmAttributes` stored
/// before `SyncInitTrigger` failed — delta, value type, value and the
/// test type — while the counter, the resolved wait value (except an
/// overflowing Relative sum, which is stored wrapped), the test the
/// trigger runs and the state stay. A stored bad value type makes a
/// later value-only change Relative; a stored bad test type is what
/// QueryAlarm reports, passes the delta-sign check, and the alarm keeps
/// firing on its old PositiveComparison.
#[test]
fn sync_change_alarm_failures_keep_what_xorg_keeps() {
    use yserver_protocol::x11::sync as s;
    const X: u32 = 0x0010_0090;
    let mut f = sync_fixture();
    let bad_counter = crate::nested::SYNC_FIRST_ERROR + s::BAD_COUNTER;
    let (value, matchh) = (x11::error::BAD_VALUE, x11::error::BAD_MATCH);
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 20));
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(
            X,
            s::CA_COUNTER | s::CA_VALUE_TYPE | s::CA_VALUE | s::CA_TEST_TYPE | s::CA_DELTA,
            &[SYNC_C1, 0, 0, 30, s::TEST_POSITIVE_COMPARISON, 0, 1],
        ),
    );
    let q = |f: &mut SyncFixture| query_alarm_fields(f, X);
    let pc = s::TEST_POSITIVE_COMPARISON;
    assert_eq!(q(&mut f), (SYNC_C1, 0, 30, pc, 1, 1, 0));
    let change = |f: &mut SyncFixture, mask: u32, values: &[u32]| {
        f.a(s::CHANGE_ALARM, &sync_alarm_body(X, mask, values));
        f.a_packets()
            .first()
            .map(error_fields)
            .map(|(code, bad, minor, _)| {
                assert_eq!(minor, u16::from(s::CHANGE_ALARM));
                (code, bad)
            })
    };
    let m = u32::MAX;
    assert_eq!(
        change(&mut f, s::CA_VALUE_TYPE | s::CA_VALUE, &[7, 0, 50]),
        Some((value, 7))
    );
    assert_eq!(q(&mut f), (SYNC_C1, 0, 30, pc, 1, 1, 0));
    assert_eq!(change(&mut f, s::CA_VALUE, &[0, 5]), None);
    assert_eq!(
        q(&mut f),
        (SYNC_C1, 0, 25, pc, 1, 1, 0),
        "stored type 7 acts Relative"
    );
    assert_eq!(
        change(
            &mut f,
            s::CA_COUNTER | s::CA_VALUE | s::CA_DELTA,
            &[0x0bad_bad0, 0, 70, 0, 3]
        ),
        Some((bad_counter, 0x0bad_bad0))
    );
    assert_eq!(q(&mut f), (SYNC_C1, 0, 25, pc, 3, 1, 0), "delta sticks");
    assert_eq!(change(&mut f, s::CA_DELTA, &[m, m]), Some((matchh, X)));
    assert_eq!(
        change(
            &mut f,
            s::CA_VALUE_TYPE | s::CA_VALUE | s::CA_TEST_TYPE,
            &[0, 0, 100, s::TEST_NEGATIVE_TRANSITION]
        ),
        Some((matchh, X))
    );
    assert_eq!(q(&mut f), (SYNC_C1, 0, 25, pc, 3, 1, 0));
    assert_eq!(change(&mut f, s::CA_TEST_TYPE, &[9]), Some((value, 9)));
    assert_eq!(q(&mut f), (SYNC_C1, 0, 25, 9, 3, 1, 0));
    assert_eq!(
        change(&mut f, s::CA_DELTA, &[m, m]),
        None,
        "no sign for test type 9"
    );
    assert_eq!(q(&mut f), (SYNC_C1, 0, 25, 9, -1, 1, 0));
    assert_eq!(change(&mut f, s::CA_DELTA, &[0, 1]), None);
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 25));
    assert_eq!(
        alarm_notifies(&f.a_packets()),
        vec![(X, 25, 25, s::ALARM_STATE_ACTIVE)]
    );
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 21));
    assert!(f.a_packets().is_empty());
    assert_eq!(q(&mut f), (SYNC_C1, 0, 26, 9, 1, 1, 0));
    f.a(s::CHANGE_ALARM, &sync_alarm_body(X, s::CA_COUNTER, &[0]));
    assert_eq!(
        alarm_notifies(&f.a_packets()),
        vec![(X, 0, 26, s::ALARM_STATE_INACTIVE)]
    );
    assert_eq!(
        change(&mut f, s::CA_VALUE_TYPE, &[s::VALUE_TYPE_RELATIVE]),
        Some((matchh, X))
    );
    assert_eq!(q(&mut f), (0, 0, 26, 9, 1, 1, s::ALARM_STATE_INACTIVE));
    assert_eq!(
        change(
            &mut f,
            s::CA_COUNTER | s::CA_VALUE_TYPE | s::CA_VALUE,
            &[SYNC_C1, s::VALUE_TYPE_RELATIVE, 0x7fff_ffff, m]
        ),
        Some((value, 0x7fff_ffff))
    );
    let wrapped = 21i64.wrapping_add(i64::MAX);
    assert_eq!(q(&mut f), (0, 0, wrapped, 9, 1, 1, s::ALARM_STATE_INACTIVE));
    assert_eq!(change(&mut f, 1 << 6, &[0]), Some((value, 0)));
    f.a(s::DESTROY_ALARM, &X.to_le_bytes());
    assert_eq!(
        alarm_notifies(&f.a_packets()),
        vec![(X, 0, wrapped, s::ALARM_STATE_DESTROYED)]
    );
}

/// Xvfb (the "bad test type" line of "AlarmNotify selection"): B's
/// ChangeAlarm {test type 9, events True} on A's alarm is BadValue(9),
/// yet B is selected (the selection precedes the check) and the alarm,
/// still running PositiveComparison, notifies B when the counter
/// reaches it; QueryAlarm reports test type 9.
#[test]
fn sync_alarm_bad_test_type_still_selects_and_fires() {
    use yserver_protocol::x11::sync as s;
    const AL: u32 = 0x0010_00a0;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 13));
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(
            AL,
            s::CA_COUNTER | s::CA_VALUE | s::CA_EVENTS,
            &[SYNC_C1, 0, 14, 0],
        ),
    );
    f.b(
        s::CHANGE_ALARM,
        &sync_alarm_body(AL, s::CA_TEST_TYPE | s::CA_EVENTS, &[9, 1]),
    );
    assert_eq!(
        error_fields(&f.b_packets()[0]),
        (x11::error::BAD_VALUE, 9, u16::from(s::CHANGE_ALARM), 142)
    );
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 14));
    assert_eq!(
        alarm_notifies(&f.b_packets()),
        vec![(AL, 14, 14, s::ALARM_STATE_ACTIVE)]
    );
    assert!(f.a_packets().is_empty());
    assert_eq!(query_alarm_fields(&mut f, AL), (SYNC_C1, 0, 15, 9, 1, 0, 0));
}

/// Every AlarmNotify in `packets` → (alarm, counter value, alarm
/// value, state).
fn alarm_notifies(packets: &[[u8; 32]]) -> Vec<(u32, i64, i64, u8)> {
    let i64_at = |p: &[u8; 32], at: usize| {
        #[allow(clippy::cast_possible_wrap)]
        let hi = le_u32(p, at) as i32;
        (i64::from(hi) << 32) | i64::from(le_u32(p, at + 4))
    };
    packets
        .iter()
        .filter(|p| p[0] == crate::nested::SYNC_FIRST_EVENT + 1)
        .map(|p| {
            assert_eq!(p[1], 1, "kind AlarmNotify");
            (le_u32(p, 4), i64_at(p, 8), i64_at(p, 16), p[28])
        })
        .collect()
}

fn sync_events_body(alarm: u32, on: u32) -> Vec<u8> {
    use yserver_protocol::x11::sync as s;
    sync_alarm_body(alarm, s::CA_EVENTS, &[on])
}

/// Xvfb (tools/sync-await-probe.c, "AlarmNotify selection"): an alarm
/// owned by A (c >= 10, delta 1, events). B and D select AlarmNotify
/// with ChangeAlarm(events=True) — any client may — and every firing
/// reaches the owner (while its own flag is set) and each selecting
/// client once. events=False by B removes B; by A clears only the
/// owner's flag (what QueryAlarm reports). A selecting client that
/// disconnects drops off the list. A non-owner may change other
/// attributes too. DestroyAlarm sends a Destroyed notify to the
/// selecting clients. An `events` value other than True/False is
/// BadValue naming it and selects nothing.
#[test]
fn sync_alarm_notify_reaches_every_client_that_selected_it() {
    use yserver_protocol::x11::sync as s;
    const AL: u32 = 0x0010_0040;
    const D: u32 = 3;
    let mut f = sync_fixture();
    let mut peer_d = install_client(&mut f.state, D);
    let d_req = |f: &mut SyncFixture, minor: u8, body: &[u8]| {
        sync_req(&mut f.state, &mut f.backend, D, minor, body);
    };
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 0));
    let mut create = AL.to_le_bytes().to_vec();
    create.extend_from_slice(
        &(s::CA_COUNTER
            | s::CA_VALUE_TYPE
            | s::CA_VALUE
            | s::CA_TEST_TYPE
            | s::CA_DELTA
            | s::CA_EVENTS)
            .to_le_bytes(),
    );
    for v in [
        SYNC_C1,
        s::VALUE_TYPE_ABSOLUTE,
        0,
        10,
        s::TEST_POSITIVE_COMPARISON,
        0,
        1,
        1,
    ] {
        create.extend_from_slice(&v.to_le_bytes());
    }
    f.a(s::CREATE_ALARM, &create);
    f.b(s::CHANGE_ALARM, &sync_events_body(AL, 1));
    d_req(&mut f, s::CHANGE_ALARM, &sync_events_body(AL, 1));
    f.b(s::CHANGE_ALARM, &sync_events_body(AL, 1));
    assert!(f.b_packets().is_empty(), "selecting sends nothing");
    assert!(f.state.sync_alarms[&AL].events, "owner flag untouched");
    assert!(f.a_packets().is_empty());

    let set = |f: &mut SyncFixture, v: i64| f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, v));
    set(&mut f, 10);
    let fired = |v: i64| vec![(AL, v, v, s::ALARM_STATE_ACTIVE)];
    assert_eq!(alarm_notifies(&f.a_packets()), fired(10));
    assert_eq!(alarm_notifies(&f.b_packets()), fired(10), "B once");
    assert_eq!(alarm_notifies(&wire_packets(&mut peer_d)), fired(10));

    f.b(s::CHANGE_ALARM, &sync_events_body(AL, 0));
    set(&mut f, 11);
    assert_eq!(alarm_notifies(&f.a_packets()), fired(11));
    assert!(f.b_packets().is_empty(), "B deselected");
    assert_eq!(alarm_notifies(&wire_packets(&mut peer_d)), fired(11));

    f.a(s::CHANGE_ALARM, &sync_events_body(AL, 0));
    set(&mut f, 12);
    assert!(f.a_packets().is_empty(), "owner flag off");
    assert!(!f.state.sync_alarms[&AL].events);
    assert_eq!(alarm_notifies(&wire_packets(&mut peer_d)), fired(12));

    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(D),
    );
    assert!(f.state.sync_alarms[&AL].event_clients.is_empty());
    set(&mut f, 13);
    assert!(f.a_packets().is_empty() && f.b_packets().is_empty());

    f.b(s::CHANGE_ALARM, &sync_events_body(AL, 9));
    assert_eq!(
        error_fields(&f.b_packets()[0]),
        (x11::error::BAD_VALUE, 9, u16::from(s::CHANGE_ALARM), 142)
    );
    assert!(f.state.sync_alarms[&AL].event_clients.is_empty());

    f.b(s::CHANGE_ALARM, &sync_events_body(AL, 1));
    set(&mut f, 14);
    assert_eq!(alarm_notifies(&f.b_packets()), fired(14));
    f.b(
        s::CHANGE_ALARM,
        &sync_alarm_body(AL, s::CA_VALUE, &[0, 100]),
    );
    assert!(f.b_packets().is_empty(), "a non-owner may change the value");
    assert_eq!(f.state.sync_alarms[&AL].wait_value, 100);

    f.a(s::DESTROY_ALARM, &AL.to_le_bytes());
    assert!(f.a_packets().is_empty());
    assert_eq!(
        alarm_notifies(&f.b_packets()),
        vec![(AL, 14, 100, s::ALARM_STATE_DESTROYED)]
    );
}

/// Xvfb: a non-owner may DestroyAlarm (owner and selecting clients get
/// the Destroyed notify); the owner's disconnect destroys its alarms,
/// and the clients that selected them get the Destroyed notify with
/// the counter's value. A destroyed counter deactivates the alarm with
/// an Inactive notify to them as well.
#[test]
fn sync_alarm_destruction_notifies_every_selecting_client() {
    use yserver_protocol::x11::sync as s;
    const E: u32 = 3;
    const CE: u32 = 0x0030_0001;
    const AE1: u32 = 0x0030_0002;
    const AE2: u32 = 0x0030_0003;
    const A2: u32 = 0x0010_0050;
    let mut f = sync_fixture();
    let mut peer_e = install_client(&mut f.state, E);
    let e_req = |f: &mut SyncFixture, minor: u8, body: &[u8]| {
        sync_req(&mut f.state, &mut f.backend, E, minor, body);
    };
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 14));
    e_req(&mut f, s::CREATE_COUNTER, &sync_counter_body(CE, 0));
    e_req(
        &mut f,
        s::CREATE_ALARM,
        &sync_alarm_body(AE1, s::CA_COUNTER | s::CA_VALUE, &[CE, 0, 50]),
    );
    e_req(
        &mut f,
        s::CREATE_ALARM,
        &sync_alarm_body(AE2, s::CA_COUNTER | s::CA_VALUE, &[SYNC_C1, 0, 50]),
    );
    f.b(s::CHANGE_ALARM, &sync_events_body(AE1, 1));
    f.b(s::CHANGE_ALARM, &sync_events_body(AE2, 1));
    f.b(s::DESTROY_ALARM, &AE1.to_le_bytes());
    let destroyed = vec![(AE1, 0, 50, s::ALARM_STATE_DESTROYED)];
    assert_eq!(alarm_notifies(&wire_packets(&mut peer_e)), destroyed);
    assert_eq!(alarm_notifies(&f.b_packets()), destroyed);
    assert!(!f.state.sync_alarms.contains_key(&AE1));

    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(E),
    );
    assert_eq!(
        alarm_notifies(&f.b_packets()),
        vec![(AE2, 14, 50, s::ALARM_STATE_DESTROYED)]
    );
    assert!(!f.state.sync_alarms.contains_key(&AE2));

    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C2, 3));
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(A2, s::CA_COUNTER | s::CA_VALUE, &[SYNC_C2, 0, 50]),
    );
    f.b(s::CHANGE_ALARM, &sync_events_body(A2, 1));
    f.a(s::DESTROY_COUNTER, &SYNC_C2.to_le_bytes());
    let inactive = vec![(A2, 3, 50, s::ALARM_STATE_INACTIVE)];
    assert_eq!(alarm_notifies(&f.a_packets()), inactive);
    assert_eq!(alarm_notifies(&f.b_packets()), inactive);
    f.a(s::DESTROY_ALARM, &A2.to_le_bytes());
    let destroyed = vec![(A2, 0, 50, s::ALARM_STATE_DESTROYED)];
    assert_eq!(alarm_notifies(&f.a_packets()), destroyed);
    assert_eq!(alarm_notifies(&f.b_packets()), destroyed);
}

/// Xvfb: CreateAlarm defaults are Xorg's (PositiveComparison, value 0,
/// delta 1, events): on a counter at 0 it fires at once and re-arms to
/// a wait value of 1. Without a counter the alarm starts Inactive and
/// silent; a ChangeAlarm on it then fires Inactive with counter value
/// 0 ("NULL counter WILL trigger in ChangeAlarm").
#[test]
fn sync_alarm_defaults_and_counterless_alarms_match_xvfb() {
    use yserver_protocol::x11::sync as s;
    const GONE: u32 = 0x0010_0060;
    const AN: u32 = 0x0020_0060;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 0));
    f.a(
        s::CREATE_ALARM,
        &sync_alarm_body(GONE, s::CA_COUNTER, &[SYNC_C1]),
    );
    assert_eq!(
        alarm_notifies(&f.a_packets()),
        vec![(GONE, 0, 0, s::ALARM_STATE_ACTIVE)]
    );
    f.a(s::DESTROY_ALARM, &GONE.to_le_bytes());
    assert_eq!(
        alarm_notifies(&f.a_packets()),
        vec![(GONE, 0, 1, s::ALARM_STATE_DESTROYED)]
    );

    f.b(s::CREATE_ALARM, &sync_alarm_body(AN, s::CA_VALUE, &[0, 5]));
    assert!(f.b_packets().is_empty());
    assert_eq!(f.state.sync_alarms[&AN].state, s::ALARM_STATE_INACTIVE);
    f.b(s::CHANGE_ALARM, &sync_events_body(AN, 1));
    assert_eq!(
        alarm_notifies(&f.b_packets()),
        vec![(AN, 0, 5, s::ALARM_STATE_INACTIVE)]
    );
    f.b(s::DESTROY_ALARM, &AN.to_le_bytes());
    assert_eq!(
        alarm_notifies(&f.b_packets()),
        vec![(AN, 0, 5, s::ALARM_STATE_DESTROYED)]
    );
}

/// A DRI3 FenceFromFD xshmfence keeps its state in memory the client
/// can trigger and reset itself (`xshmfence_trigger` /
/// `xshmfence_reset`). Xorg's shm fence (`misyncshm.c`) answers every
/// `CheckTriggered` from that memory — QueryFence, the AwaitFence
/// epilogue, ResetFence's BadMatch test, and `miSyncTriggerFence`'s
/// re-check — and never from the server's own bit; ResetFence resets
/// the memory and DestroyFence triggers it before unmapping. Nothing in
/// Xorg watches the memory, so an AwaitFence already suspended stays
/// suspended when the client triggers it in memory, until a
/// TriggerFence request or the fence's destruction. Xvfb has no DRI3,
/// so this follows the source (`miSyncShmFenceCheckTriggered`,
/// `miSyncShmFenceReset`, `miSyncShmScreenDestroyFence`,
/// `SyncAwaitEpilogue`, `miSyncTriggerFence`).
#[test]
fn sync_shm_fence_state_is_read_from_shared_memory() {
    use yserver_protocol::x11::sync as s;
    const F: u32 = 0x0010_0070;
    const G: u32 = 0x0010_0071;
    let mut f = sync_fixture();
    for fence in [F, G] {
        f.state.sync_fences.insert(
            fence,
            crate::server::SyncFence {
                owner: ClientId(SYNC_A),
                triggered: false,
            },
        );
        f.backend.shm_fences.insert(fence, false);
    }
    let query = |f: &mut SyncFixture| {
        f.b(s::QUERY_FENCE, &F.to_le_bytes());
        let reply = f.b_packets();
        assert_eq!(reply[0][0], 1, "QueryFence reply");
        reply[0][8] != 0
    };

    // The client triggers it in memory: QueryFence sees it and an
    // AwaitFence returns at once.
    f.backend.shm_fences.insert(F, true);
    assert!(query(&mut f));
    f.b(s::AWAIT_FENCE, &F.to_le_bytes());
    assert!(!f.suspended());

    // The client resets it in memory after the server triggered it:
    // the memory wins over the server's bit.
    f.a(s::TRIGGER_FENCE, &F.to_le_bytes());
    assert!(f.state.sync_fences[&F].triggered);
    f.backend.shm_fences.insert(F, false);
    assert!(!query(&mut f));
    f.b(s::RESET_FENCE, &F.to_le_bytes());
    assert_eq!(
        error_fields(&f.b_packets()[0]),
        (x11::error::BAD_MATCH, F, u16::from(s::RESET_FENCE), 142),
        "untriggered in memory"
    );
    f.b(s::AWAIT_FENCE, &F.to_le_bytes());
    assert!(f.suspended(), "untriggered in memory");

    // A trigger in memory alone wakes nothing (Xorg never looks);
    // a TriggerFence request does.
    f.backend.shm_fences.insert(F, true);
    assert!(f.suspended());
    f.a(s::TRIGGER_FENCE, &F.to_le_bytes());
    assert!(!f.suspended());

    // ResetFence of a fence triggered in memory resets the memory.
    f.b(s::RESET_FENCE, &F.to_le_bytes());
    assert!(f.b_packets().is_empty());
    assert_eq!(f.backend.shm_fences.get(&F), Some(&false));
    assert!(!query(&mut f));

    // DestroyFence triggers the memory (releasing client waiters) and
    // unmaps it; so does the owner's disconnect.
    f.a(s::DESTROY_FENCE, &F.to_le_bytes());
    assert_eq!(f.backend.destroyed_shm_fences, vec![(F, true)]);
    assert!(!f.backend.shm_fences.contains_key(&F));
    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(SYNC_A),
    );
    assert_eq!(f.backend.destroyed_shm_fences, vec![(F, true), (G, true)]);
    assert!(f.backend.shm_fences.is_empty());
}

/// SERVERTIME awaits wake when the clock reaches the value (Xvfb: an
/// absolute now+300 PositiveComparison blocked ~300 ms and reported
/// wait == value), and the loop gets a deadline for it.
#[test]
fn sync_await_on_servertime_fires_when_the_clock_gets_there() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    let now = i64::from(f.state.timestamp_now());
    f.b(
        s::AWAIT,
        &sync_wait(
            s::SERVERTIME_COUNTER,
            s::VALUE_TYPE_ABSOLUTE,
            now + 300,
            s::TEST_POSITIVE_COMPARISON,
            0,
        ),
    );
    assert!(f.suspended());
    let deadline = crate::core_loop::sync_await::system_counter_deadline(&f.state)
        .expect("a SERVERTIME deadline");
    let wait = deadline.saturating_duration_since(std::time::Instant::now());
    assert!(
        wait > std::time::Duration::from_millis(200)
            && wait <= std::time::Duration::from_millis(300),
        "deadline ~300 ms out, got {wait:?}"
    );
    crate::core_loop::sync_await::evaluate_servertime(&mut f.state);
    assert!(f.suspended(), "not there yet");
    // Let 400 ms of server time pass. Until the loop evaluates it, the
    // passed value must read as due now — never as "no deadline", or
    // an idle loop would block past it.
    f.state.start_instant -= std::time::Duration::from_millis(400);
    let due = crate::core_loop::sync_await::system_counter_deadline(&f.state)
        .expect("still pending until evaluated");
    assert!(due <= std::time::Instant::now());
    crate::core_loop::sync_await::evaluate_servertime(&mut f.state);
    assert!(!f.suspended());
    let (counter, wait_value, value, count, destroyed) = counter_notify_fields(&f.b_packets()[0]);
    assert_eq!(
        (counter, wait_value, count, destroyed),
        (s::SERVERTIME_COUNTER, now + 300, 0, false)
    );
    assert!(value >= now + 300);
    assert!(crate::core_loop::sync_await::system_counter_deadline(&f.state).is_none());
}

/// SERVERTIME alarms fire too (Xvfb: an absolute now+200
/// PositiveComparison alarm with delta 0 sent AlarmNotify after ~200 ms
/// with state Inactive and counter >= alarm value).
#[test]
fn sync_alarm_on_servertime_fires_when_the_clock_gets_there() {
    use yserver_protocol::x11::sync as s;
    const ALARM: u32 = 0x0010_0030;
    let mut f = sync_fixture();
    let now = i64::from(f.state.timestamp_now());
    let mut body = ALARM.to_le_bytes().to_vec();
    let mask = s::CA_COUNTER | s::CA_VALUE_TYPE | s::CA_VALUE | s::CA_TEST_TYPE | s::CA_DELTA;
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&s::SERVERTIME_COUNTER.to_le_bytes());
    body.extend_from_slice(&s::VALUE_TYPE_ABSOLUTE.to_le_bytes());
    body.extend_from_slice(&sync_i64(now + 200));
    body.extend_from_slice(&s::TEST_POSITIVE_COMPARISON.to_le_bytes());
    body.extend_from_slice(&sync_i64(0));
    f.b(s::CREATE_ALARM, &body);
    assert!(f.b_packets().is_empty(), "not due yet");
    assert!(crate::core_loop::sync_await::system_counter_deadline(&f.state).is_some());
    f.state.start_instant -= std::time::Duration::from_millis(300);
    crate::core_loop::sync_await::evaluate_servertime(&mut f.state);
    let packets = f.b_packets();
    assert_eq!(packets.len(), 1);
    assert_eq!(
        packets[0][0],
        crate::nested::SYNC_FIRST_EVENT + 1,
        "AlarmNotify"
    );
    assert_eq!(packets[0][28], s::ALARM_STATE_INACTIVE);
    assert_eq!(f.state.sync_alarms[&ALARM].state, s::ALARM_STATE_INACTIVE);
}

/// IDLETIME awaits: a positive test wakes when idle time reaches it; a
/// negative transition wakes on input (idle drops to 0).
#[test]
fn sync_await_on_idletime_fires_on_idle_and_on_input() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.state.dpms.last_activity = std::time::Instant::now();
    f.b(
        s::AWAIT,
        &sync_wait(
            s::IDLETIME_COUNTER,
            s::VALUE_TYPE_ABSOLUTE,
            5_000,
            s::TEST_POSITIVE_TRANSITION,
            0,
        ),
    );
    assert!(f.suspended());
    assert!(crate::core_loop::sync_await::system_counter_deadline(&f.state).is_some());
    f.state.dpms.last_activity -= std::time::Duration::from_secs(6);
    crate::core_loop::run::evaluate_idletime_alarms_post_poll(&mut f.state, &mut f.backend);
    assert!(!f.suspended());
    let (counter, wait_value, _, _, _) = counter_notify_fields(&f.b_packets()[0]);
    assert_eq!((counter, wait_value), (s::IDLETIME_COUNTER, 5_000));

    f.b(
        s::AWAIT,
        &sync_wait(
            s::IDLETIME_COUNTER,
            s::VALUE_TYPE_ABSOLUTE,
            1_000,
            s::TEST_NEGATIVE_TRANSITION,
            0,
        ),
    );
    assert!(f.suspended(), "idle is ~6 s, no downward crossing yet");
    evaluate_idletime_negative_alarms_on_input_wake(&mut f.state, 2, 6_000, 6_000);
    assert!(!f.suspended());
}

/// The awaiting client's own disconnect drops its await.
#[test]
fn sync_await_dies_with_its_client() {
    use yserver_protocol::x11::sync as s;
    let mut f = sync_fixture();
    f.a(s::CREATE_COUNTER, &sync_counter_body(SYNC_C1, 0));
    f.b(
        s::AWAIT,
        &sync_wait(
            SYNC_C1,
            s::VALUE_TYPE_ABSOLUTE,
            5,
            s::TEST_POSITIVE_COMPARISON,
            0,
        ),
    );
    assert!(f.suspended());
    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(SYNC_B),
    );
    assert!(f.state.sync_awaits.is_empty());
    f.a(s::SET_COUNTER, &sync_counter_body(SYNC_C1, 9));
    assert!(f.state.sync_counters.contains_key(&SYNC_C1));
}

/// Read the XSync i64 counter value from a QueryCounter reply.
/// Protocol encoding: hi(INT32 LE at bytes[8..12]) || lo(CARD32 LE at
/// bytes[12..16]) — NOT a raw 8-byte little-endian integer.
fn read_query_counter_reply_value(bytes: &[u8]) -> i64 {
    let hi = i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let lo = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    (i64::from(hi) << 32) | i64::from(lo)
}

#[test]
fn query_counter_idletime_returns_elapsed_since_last_activity_not_uptime() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);

    let header = RequestHeader {
        opcode: 142,
        data: x11sync::QUERY_COUNTER,
        length_units: 2,
    };
    let body = x11sync::IDLETIME_COUNTER.to_le_bytes();
    let _ = handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let bytes = read_all_available(&mut peer);
    // QueryCounter reply: tag(1) data(1) seq(2) length(4) value:i64(8) pad(16)
    assert_eq!(bytes[0], 1, "reply tag");
    let value = read_query_counter_reply_value(&bytes);
    assert!(
        (29_000..=35_000).contains(&value),
        "IDLETIME ≈ 30_000ms ± scheduling slack; got {value}"
    );
}

#[test]
fn query_counter_idletime_device_vcp_uses_per_device_baseline() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Global was idle 60s ago, but the pointer device is fresh.
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(60);
    state.per_device_last_activity.insert(
        2, /* VCP */
        std::time::Instant::now() - Duration::from_millis(500),
    );

    let header = RequestHeader {
        opcode: 142,
        data: x11sync::QUERY_COUNTER,
        length_units: 2,
    };
    let body = x11sync::IDLETIME_DEVICE_VCP.to_le_bytes();
    let _ = handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let bytes = read_all_available(&mut peer);
    let value = read_query_counter_reply_value(&bytes);
    assert!(
        (400..=5_000).contains(&value),
        "VCP IDLETIME ≈ 500ms; got {value}"
    );
}

#[test]
fn query_counter_idletime_device_vck_uses_per_device_baseline() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(60);
    state.per_device_last_activity.insert(
        3, /* VCK */
        std::time::Instant::now() - Duration::from_millis(200),
    );

    let header = RequestHeader {
        opcode: 142,
        data: x11sync::QUERY_COUNTER,
        length_units: 2,
    };
    let body = x11sync::IDLETIME_DEVICE_VCK.to_le_bytes();
    let _ = handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let bytes = read_all_available(&mut peer);
    let value = read_query_counter_reply_value(&bytes);
    assert!(
        (100..=3_000).contains(&value),
        "VCK IDLETIME ≈ 200ms; got {value}"
    );
}

#[test]
fn evaluate_alarms_positive_transition_with_delta_zero_stays_active() {
    // Regression: Xorg sync.c:548-555 — only delta=0 + Comparison
    // goes Inactive. Transitions with delta=0 must stay Active so
    // the next crossing re-fires.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let counter = 0x1000;
    state.sync_counters.insert(
        counter,
        crate::server::SyncCounter {
            owner: ClientId(1),
            value: 0,
        },
    );
    let alarm_id = 0x2000;
    state.sync_alarms.insert(
        alarm_id,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter,
            wait_value: 100,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 100,
            check_type: x11sync::TEST_POSITIVE_TRANSITION,
        },
    );

    // Counter transition from 50 → 150 crosses the trigger.
    evaluate_alarms_for_counter(&mut state, counter, 50, 150);

    let after = &state.sync_alarms[&alarm_id];
    assert_eq!(
        after.state,
        x11sync::ALARM_STATE_ACTIVE,
        "PositiveTransition + delta=0 must stay Active (Xorg sync.c:548-555)"
    );
    assert_eq!(
        after.wait_value, 100,
        "wait_value unchanged for delta=0 Transition"
    );
}

#[test]
fn evaluate_alarms_positive_comparison_with_delta_zero_goes_inactive() {
    // Companion test: PositiveComparison + delta=0 SHOULD go Inactive.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let counter = 0x1000;
    state.sync_counters.insert(
        counter,
        crate::server::SyncCounter {
            owner: ClientId(1),
            value: 0,
        },
    );
    let alarm_id = 0x2000;
    state.sync_alarms.insert(
        alarm_id,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter,
            wait_value: 100,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_COMPARISON,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 100,
            check_type: x11sync::TEST_POSITIVE_COMPARISON,
        },
    );

    evaluate_alarms_for_counter(&mut state, counter, 50, 150);

    let after = &state.sync_alarms[&alarm_id];
    assert_eq!(after.state, x11sync::ALARM_STATE_INACTIVE);
}

#[test]
fn evaluate_alarms_re_arm_overflow_transitions_alarm_to_inactive() {
    // Xorg sync.c:589-597 — re-arm overflow on i64::checked_add
    // must transition the alarm to Inactive and leave wait_value
    // unchanged.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let counter = 0x1000;
    state.sync_counters.insert(
        counter,
        crate::server::SyncCounter {
            owner: ClientId(1),
            value: 0,
        },
    );
    let alarm_id = 0x2000;
    // Start near i64::MAX so adding delta overflows on the very
    // first re-arm step. Use PositiveTransition so the trigger fires
    // when new >= wait_value.
    let near_max = i64::MAX - 10;
    state.sync_alarms.insert(
        alarm_id,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter,
            wait_value: near_max,
            delta: 100, // would overflow on first add
            test_type: x11sync::TEST_POSITIVE_TRANSITION,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: near_max,
            check_type: x11sync::TEST_POSITIVE_TRANSITION,
        },
    );

    // Crossing: old < wait_value, new >= wait_value → trigger fires.
    evaluate_alarms_for_counter(&mut state, counter, near_max - 1, near_max + 5);

    let after = &state.sync_alarms[&alarm_id];
    assert_eq!(
        after.state,
        x11sync::ALARM_STATE_INACTIVE,
        "overflow on re-arm → Inactive"
    );
    assert_eq!(
        after.wait_value, near_max,
        "wait_value unchanged on overflow (Xorg sync.c:589-597)"
    );
}

/// Encode an INT64 value in XSync wire format: hi(INT32 LE) || lo(CARD32 LE).
/// This is NOT the same as `i64::to_le_bytes()` (which is lo-first).
fn xsync_i64(v: i64) -> [u8; 8] {
    #[allow(clippy::cast_possible_truncation)]
    let hi = (v >> 32) as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = v as u32;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&hi.to_le_bytes());
    out[4..].copy_from_slice(&lo.to_le_bytes());
    out
}

#[test]
fn relative_alarm_value_on_idletime_resolves_against_current_idle() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Pre-condition: already idle 5s.
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(5);

    // CreateAlarm relative-value=10_000 → wait_value should resolve
    // to current_idle (~5_000) + 10_000 = ~15_000.
    let alarm_id: u32 = 0x2000_0002;
    let mut body = Vec::new();
    body.extend_from_slice(&alarm_id.to_le_bytes());
    let mask: u32 = x11sync::CA_COUNTER
        | x11sync::CA_VALUE_TYPE
        | x11sync::CA_VALUE
        | x11sync::CA_TEST_TYPE
        | x11sync::CA_DELTA
        | x11sync::CA_EVENTS;
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&x11sync::IDLETIME_COUNTER.to_le_bytes());
    body.extend_from_slice(&x11sync::VALUE_TYPE_RELATIVE.to_le_bytes()); // u32
    body.extend_from_slice(&xsync_i64(10_000)); // value = 10_000 relative
    body.extend_from_slice(&x11sync::TEST_POSITIVE_TRANSITION.to_le_bytes());
    body.extend_from_slice(&xsync_i64(0)); // delta = 0
    body.push(1); // events = true
    body.extend_from_slice(&[0u8; 3]);

    let header = RequestHeader {
        opcode: 142,
        data: x11sync::CREATE_ALARM,
        length_units: u32::try_from(2 + body.len() / 4).unwrap(),
    };
    let _ = handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let alarm = state.sync_alarms.get(&alarm_id).expect("alarm exists");
    assert!(
        alarm.wait_value >= 14_500 && alarm.wait_value <= 16_000,
        "wait_value ≈ current_idle(5000) + 10000 = 15000; got {}",
        alarm.wait_value
    );
}

#[test]
fn create_alarm_on_idletime_with_comparison_already_true_fires_immediately() {
    // Xorg sync.c:1772-1775 — create-time trigger eval passes
    // (old=current, new=current). PositiveTransition requires
    // `old < wait <= new` so it cannot fire on a same-value pair;
    // only Comparison test types can fire at create time.
    // PositiveComparison fires when `new >= wait`, which is
    // exactly the "client opens an alarm while user is already
    // idle past the threshold" smoke we want to verify.
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Pre-condition: already idle past the alarm threshold.
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);

    let alarm_id: u32 = 0x2000_0001;
    let mut body = Vec::new();
    body.extend_from_slice(&alarm_id.to_le_bytes());
    let mask: u32 = x11sync::CA_COUNTER
        | x11sync::CA_VALUE_TYPE
        | x11sync::CA_VALUE
        | x11sync::CA_TEST_TYPE
        | x11sync::CA_DELTA
        | x11sync::CA_EVENTS;
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&x11sync::IDLETIME_COUNTER.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // value_type = Absolute
    body.extend_from_slice(&xsync_i64(60_000)); // value = 60_000
    body.extend_from_slice(&x11sync::TEST_POSITIVE_COMPARISON.to_le_bytes());
    body.extend_from_slice(&xsync_i64(0)); // delta = 0 → goes Inactive on fire
    body.push(1); // events = true
    body.extend_from_slice(&[0u8; 3]);

    let header = RequestHeader {
        opcode: 142,
        data: x11sync::CREATE_ALARM,
        length_units: u32::try_from(2 + body.len() / 4).unwrap(),
    };
    let _ = handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let bytes = read_all_available(&mut peer);
    // AlarmNotify event: type = SYNC_FIRST_EVENT(83) + ALARM_NOTIFY_KIND(1) = 84
    assert!(
        bytes.contains(&84),
        "AlarmNotify must fire at create-time for an already-idle PositiveComparison alarm; got {:?}",
        bytes
    );
    // Comparison + delta=0 transitions to Inactive on fire.
    assert_eq!(
        state.sync_alarms[&alarm_id].state,
        x11sync::ALARM_STATE_INACTIVE
    );
}

#[test]
fn create_alarm_on_idletime_with_pos_transition_does_not_fire_at_create() {
    // Companion test: PositiveTransition cannot fire at create-time
    // (Xorg sync.c:1772-1775 — old==new). The alarm sits Active
    // waiting for the post-poll evaluator or input wake to drive
    // the (old, new) pair across the threshold.
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);

    let alarm_id: u32 = 0x2000_0003;
    let mut body = Vec::new();
    body.extend_from_slice(&alarm_id.to_le_bytes());
    let mask: u32 = x11sync::CA_COUNTER
        | x11sync::CA_VALUE_TYPE
        | x11sync::CA_VALUE
        | x11sync::CA_TEST_TYPE
        | x11sync::CA_DELTA
        | x11sync::CA_EVENTS;
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&x11sync::IDLETIME_COUNTER.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&xsync_i64(60_000));
    body.extend_from_slice(&x11sync::TEST_POSITIVE_TRANSITION.to_le_bytes());
    body.extend_from_slice(&xsync_i64(0));
    body.push(1);
    body.extend_from_slice(&[0u8; 3]);

    let header = RequestHeader {
        opcode: 142,
        data: x11sync::CREATE_ALARM,
        length_units: u32::try_from(2 + body.len() / 4).unwrap(),
    };
    let _ = handle_sync_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let bytes = read_all_available(&mut peer);
    assert!(
        !bytes.contains(&84),
        "PositiveTransition with old==new at create-time must not fire"
    );
    assert_eq!(
        state.sync_alarms[&alarm_id].state,
        x11sync::ALARM_STATE_ACTIVE,
        "alarm stays Active, waiting for evaluator/input to drive the transition"
    );
}
