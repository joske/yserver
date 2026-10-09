use super::*;
use proptest::prelude::*;
use std::{io::Read, os::unix::net::UnixStream};

#[test]
fn float_atom_is_pre_interned_at_server_init() {
    let state = ServerState::new();
    assert_ne!(state.float_atom.0, 0, "FLOAT must be interned at startup");
    // Re-interning must hit the cache and return the same id.
    let mut state = state;
    let again = state.atoms.intern("FLOAT", true);
    assert_eq!(again, state.float_atom);
}

#[test]
fn first_client_base_is_above_root_resources() {
    let mut a = IdAllocator::new();
    let (base, mask) = a.allocate().expect("first allocate");
    assert_eq!(base, 0x0010_0000);
    assert_eq!(mask, 0x000F_FFFF);
}

#[test]
fn allocate_increments_by_first_client_base() {
    let mut a = IdAllocator::new();
    let (b1, _) = a.allocate().unwrap();
    let (b2, _) = a.allocate().unwrap();
    assert_eq!(b2 - b1, FIRST_CLIENT_BASE);
}

#[test]
fn release_recycles_base_for_next_allocate() {
    let mut a = IdAllocator::new();
    let (b1, _) = a.allocate().unwrap();
    let (b2, _) = a.allocate().unwrap();
    a.release(b1);
    let (b3, _) = a.allocate().unwrap();
    assert_eq!(
        b3, b1,
        "released base must be reused before bumping next_base"
    );
    let (b4, _) = a.allocate().unwrap();
    assert_eq!(
        b4,
        b2 + FIRST_CLIENT_BASE,
        "fresh base resumes from next_base"
    );
}

#[test]
fn release_ignores_unaligned_or_below_first_base() {
    let mut a = IdAllocator::new();
    let (b1, _) = a.allocate().unwrap();
    a.release(b1 | 0x42); // unaligned (low bits set)
    a.release(0); // below FIRST_CLIENT_BASE
    a.release(0x1234); // below FIRST_CLIENT_BASE
    // Free list rejected all three; next allocate falls through to monotonic.
    let (b2, _) = a.allocate().unwrap();
    assert_eq!(b2, b1 + FIRST_CLIENT_BASE);
}

#[test]
fn release_survives_u32_overflow_threshold() {
    // Drain the monotonic counter to the verge of overflow, then
    // confirm a release-and-reallocate keeps working past the
    // point where a non-recycling allocator would return None.
    let mut a = IdAllocator::new();
    let mut bases = Vec::new();
    while let Some((b, _)) = a.allocate() {
        bases.push(b);
    }
    // u32::MAX / FIRST_CLIENT_BASE = 4095, but `checked_add` rejects the
    // step that *would* land on 4095 * FCB because the *next* base would
    // overflow. Net successful allocates = 4094 (bases 1*FCB through
    // 4094*FCB).
    assert_eq!(
        bases.len(),
        4094,
        "successful monotonic allocates before overflow"
    );
    assert!(
        a.allocate().is_none(),
        "next monotonic allocate must overflow"
    );
    let recycled = bases.pop().unwrap();
    a.release(recycled);
    let (reused, _) = a
        .allocate()
        .expect("recycled base allocates after overflow");
    assert_eq!(reused, recycled);
    assert!(
        a.allocate().is_none(),
        "no more free + monotonic overflowed"
    );
}

#[test]
fn validate_owned_accepts_ids_in_range() {
    let (base, mask) = (0x0020_0000, 0x000F_FFFF);
    assert!(IdAllocator::validate_owned(base, base, mask));
    assert!(IdAllocator::validate_owned(base | mask, base, mask));
    assert!(IdAllocator::validate_owned(base + 0x42, base, mask));
}

#[test]
fn validate_owned_rejects_ids_outside_range() {
    let (base, mask) = (0x0020_0000, 0x000F_FFFF);
    assert!(!IdAllocator::validate_owned(0x0010_0000, base, mask));
    assert!(!IdAllocator::validate_owned(0x0030_0000, base, mask));
    assert!(!IdAllocator::validate_owned(0x0000_0100, base, mask));
}

proptest! {
    #[test]
    fn pairwise_non_overlap(n in 1usize..256) {
        let mut a = IdAllocator::new();
        let mut ranges = Vec::with_capacity(n);
        for _ in 0..n {
            ranges.push(a.allocate().expect("range"));
        }
        for (i, (b1, m1)) in ranges.iter().enumerate() {
            for (b2, m2) in ranges.iter().skip(i + 1) {
                let lo1 = *b1;
                let hi1 = b1 | m1;
                let lo2 = *b2;
                let hi2 = b2 | m2;
                prop_assert!(hi1 < lo2 || hi2 < lo1, "overlap {:x}..={:x} vs {:x}..={:x}", lo1, hi1, lo2, hi2);
            }
        }
    }

    #[test]
    fn mask_covers_assigned_bits(n in 1usize..64) {
        let mut a = IdAllocator::new();
        for _ in 0..n {
            let (base, mask) = a.allocate().unwrap();
            prop_assert_eq!(base & mask, 0);
        }
    }

    #[test]
    fn allocated_bases_above_root_range(n in 1usize..64) {
        let mut a = IdAllocator::new();
        for _ in 0..n {
            let (base, _) = a.allocate().unwrap();
            prop_assert!(base >= 0x0010_0000);
        }
    }

    #[test]
    fn validate_round_trip(seed in 0u32..256, offset in 0u32..=PER_CLIENT_MASK) {
        let mut a = IdAllocator::new();
        for _ in 0..seed { a.allocate().unwrap(); }
        let (base, mask) = a.allocate().unwrap();
        let id = base + offset;
        prop_assert!(IdAllocator::validate_owned(id, base, mask));
        let other = base.wrapping_add(0x0010_0000).wrapping_add(offset);
        prop_assert!(!IdAllocator::validate_owned(other, base, mask));
    }
}

#[test]
fn subscribers_returns_clients_with_bit_set() {
    let mut state = ServerState::new();
    state.clients.insert(
        1,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    state.clients.insert(
        2,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0020_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    // PropertyChange = 0x0040_0000
    let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
    assert_eq!(subs.len(), 1);
}

#[test]
fn subscribers_omits_other_windows() {
    let mut state = ServerState::new();
    state.clients.insert(
        1,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x200), 0xFFFF_FFFF)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
    assert!(subs.is_empty());
}

#[test]
fn xi2_pointer_mask_matches_exact_and_wildcard_devices() {
    for deviceid in [2u16, 1, 0] {
        let client = ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::from([((ResourceId(0x100), deviceid), 1 << 4)]),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        };

        assert_eq!(
            xi2_mask_for_client(&client, ResourceId(0x100), ResourceId(0x100), &[2, 1, 0]),
            1 << 4
        );
    }
}

/// Issue #72 at the mask layer: a client that splits its XI2
/// selection across device wildcards on one window (SDL2/lite-xl:
/// Motion under `XIAllMasterDevices(1)`, buttons under
/// `XIAllDevices(0)`) must get the OR of BOTH masks, not just the
/// first-matching device's. First-match returned only the device-1
/// mask (no button bit) and starved lite-xl of every button event.
#[test]
fn xi2_mask_ors_masks_split_across_device_wildcards() {
    let win = ResourceId(0x0080_0037);
    let client = ClientState {
        writer: make_test_writer(),
        byte_order: ClientByteOrder::LittleEndian,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: 0x0010_0000,
        resource_id_mask: 0x000F_FFFF,
        event_masks: HashMap::new(),
        save_set: HashSet::new(),
        big_requests_enabled: false,
        xi2_masks: HashMap::from([
            // XIAllMasterDevices(1): motion/enter/touch/gesture, NO buttons.
            ((win, 1u16), 0x381c_00c0u64),
            // XIAllDevices(0): includes ButtonPress(4)/ButtonRelease(5).
            ((win, 0u16), 0x19f2u64),
        ]),
        xi1_event_classes: HashSet::new(),
        xi1_window_event_classes: HashMap::new(),
        outbound: std::collections::VecDeque::new(),
        watching_writable: false,
        write_failed: false,
        focused_window: crate::resources::ROOT_WINDOW,
        reader_control: None,
        is_local: true,
        fd_passing: true,
    };

    // Button bits come from device 0; motion bit from either. First-match
    // would have returned only device 1's mask (no button bits at all).
    let mask = xi2_mask_for_client(&client, win, win, &[4, 2, 1, 0]);
    assert!(
        mask & (1 << 4) != 0,
        "issue #72: ButtonPress bit under XIAllDevices(0) must survive the OR"
    );
    assert!(
        mask & (1 << 5) != 0,
        "issue #72: ButtonRelease bit under XIAllDevices(0) must survive the OR"
    );
    assert!(mask & (1 << 6) != 0, "Motion bit must survive the OR");
}

#[test]
fn xi2_keyboard_mask_matches_exact_and_wildcard_devices() {
    for deviceid in [3u16, 1, 0] {
        let client = ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::from([((ResourceId(0x100), deviceid), 1 << 2)]),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        };

        assert_eq!(
            xi2_mask_for_client(&client, ResourceId(0x100), ResourceId(0x100), &[3, 1, 0]),
            1 << 2
        );
    }
}

#[test]
fn subscribers_omits_disconnected_client() {
    let mut state = ServerState::new();
    state.clients.insert(
        1,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
    state.clients.remove(&1);
    assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
}

#[test]
fn subscribers_intersecting_matches_any_selected_bit() {
    let mut state = ServerState::new();
    state.clients.insert(
        1,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0b1010)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    state.clients.insert(
        2,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0020_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0b0100)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );

    assert_eq!(
        state
            .subscribers_intersecting(ResourceId(0x100), 0b0010)
            .len(),
        1
    );
    assert_eq!(
        state
            .subscribers_intersecting(ResourceId(0x100), 0b1100)
            .len(),
        2
    );
    assert!(
        state
            .subscribers_intersecting(ResourceId(0x100), 0b0001)
            .is_empty()
    );
}

#[test]
fn client_target_returns_connected_client() {
    let mut state = ServerState::new();
    state.clients.insert(
        7,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0x1234)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );

    assert!(state.client_target(ClientId(7)).is_some());
    assert!(state.client_target(ClientId(8)).is_none());
}

fn make_test_writer() -> Arc<Mutex<Transport>> {
    let (a, _b) = UnixStream::pair().expect("socketpair");
    Arc::new(Mutex::new(Transport::Unix(a)))
}

#[test]
fn xi_dynamic_reset_nested_host_pointer_uses_master_xi_ids() {
    let mut state = ServerState::new();
    let (xtest_writer, mut xtest_peer) = Transport::capture_pair();
    let (master_writer, mut master_peer) = Transport::capture_pair();
    for (client_id, writer, selected_device) in [
        (1, xtest_writer, crate::xinput::DEVICEID_XTEST_POINTER),
        (2, master_writer, crate::xinput::DEVICEID_MASTER_POINTER),
    ] {
        state.clients.insert(
            client_id,
            ClientState {
                writer: Arc::new(Mutex::new(writer)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: u32::MAX,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::from([(
                    (crate::resources::ROOT_WINDOW, selected_device),
                    (1 << 6) | (1 << 17),
                )]),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
    }

    let state = Mutex::new(state);
    pointer_event_fanout(
        &state,
        &HashMap::from([(0xCAFE, crate::resources::ROOT_WINDOW)]),
        crate::host_x11::HostPointerEvent {
            origin: crate::core_loop::InputOrigin::NestedHost,
            kind: crate::host_x11::PointerEventKind::MotionNotify,
            host_xid: 0xCAFE,
            detail: 0,
            time: 1,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 1,
            raw_dy: 2,
            tree_change: false,
        },
    );

    xtest_peer
        .set_nonblocking(true)
        .expect("nonblocking XTEST peer");
    let mut xtest_events = Vec::new();
    let mut buf = [0; 256];
    loop {
        match xtest_peer.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => xtest_events.extend_from_slice(&buf[..count]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("read XTEST events: {err}"),
        }
    }
    assert!(
        xtest_events.is_empty(),
        "nested host input must not reach a client that selected only XTEST pointer 4"
    );

    master_peer
        .set_nonblocking(true)
        .expect("nonblocking master peer");
    let mut master_events = Vec::new();
    loop {
        match master_peer.read(&mut buf) {
            Ok(0) => break,
            Ok(count) => master_events.extend_from_slice(&buf[..count]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("read master events: {err}"),
        }
    }
    let mut ids = Vec::new();
    let mut offset = 0;
    while offset + 32 <= master_events.len() {
        assert_eq!(master_events[offset], 35, "GenericEvent");
        let length = u32::from_le_bytes(
            master_events[offset + 4..offset + 8]
                .try_into()
                .expect("event length"),
        ) as usize;
        let evtype = u16::from_le_bytes(
            master_events[offset + 8..offset + 10]
                .try_into()
                .expect("event type"),
        );
        let device_id = u16::from_le_bytes(
            master_events[offset + 10..offset + 12]
                .try_into()
                .expect("device id"),
        );
        let source_offset = if evtype == 17 { 20 } else { 52 };
        let source_id = u16::from_le_bytes(
            master_events[offset + source_offset..offset + source_offset + 2]
                .try_into()
                .expect("source id"),
        );
        ids.push((evtype, device_id, source_id));
        offset += 32 + length * 4;
    }
    assert_eq!(offset, master_events.len(), "event stream fully consumed");
    assert_eq!(
        ids,
        vec![
            (
                17,
                crate::xinput::DEVICEID_MASTER_POINTER,
                crate::xinput::DEVICEID_MASTER_POINTER
            ),
            (
                6,
                crate::xinput::DEVICEID_MASTER_POINTER,
                crate::xinput::DEVICEID_MASTER_POINTER
            ),
        ],
        "nested host raw and device pointer forms use master IDs",
    );
    let final_state = state.lock().expect("server state");
    assert_eq!(final_state.buttons_down, 0);
    assert_eq!(
        final_state
            .xi_devices
            .device(crate::xinput::DEVICEID_XTEST_POINTER)
            .map(|device| device.buttons_down),
        Some(0),
        "nested host input does not change XTEST held-button state",
    );
    assert!(final_state.xi_devices.source_ids().is_empty());
}

#[test]
fn unmap_notify_fanout_reaches_only_subscribed_clients() {
    use yserver_protocol::x11::{SequenceNumber, encode_unmap_notify_event};

    // Client A: StructureNotify on window 0x100.
    let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client B: KeyPress only on window 0x100 (NOT StructureNotify).
    let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");

    let mut state = ServerState::new();
    state.clients.insert(
        1,
        ClientState {
            writer: Arc::new(Mutex::new(Transport::Unix(a_writer_local))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0002_0000)]), // StructureNotify
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    state.clients.insert(
        2,
        ClientState {
            writer: Arc::new(Mutex::new(Transport::Unix(b_writer_local))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0020_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]), // KeyPress
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );

    let subs = state.subscribers(ResourceId(0x100), 0x0002_0000);
    assert_eq!(subs.len(), 1, "only client A should be subscribed");

    let target = &subs[0];
    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
    let mut buf = Vec::with_capacity(32);
    encode_unmap_notify_event(
        &mut buf,
        seq,
        target.byte_order,
        ResourceId(0x100),
        ResourceId(0x100),
        false,
    );
    assert_eq!(buf[0], 18, "wire byte 0 is UnmapNotify");
    assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
    assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
    assert_eq!(buf[12], 0, "from_configure = false");
}

#[test]
fn drop_window_subscriptions_removes_entries_for_destroyed_windows() {
    let mut state = ServerState::new();
    let xi1_class_xtest_pointer = (u32::from(crate::xinput::DEVICEID_XTEST_POINTER) << 8)
        | u32::from(XI_FIRST_EVENT + crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET);
    let xi1_class_xtest_keyboard = (u32::from(crate::xinput::DEVICEID_XTEST_KEYBOARD) << 8)
        | u32::from(XI_FIRST_EVENT + crate::xinput::XI_DEVICE_PROPERTY_NOTIFY_OFFSET);
    state.clients.insert(
        1,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([
                (ResourceId(0x100), 0x0040_0000),
                (ResourceId(0x200), 0x0040_0000),
            ]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::from([
                (
                    (ResourceId(0x100), crate::xinput::DEVICEID_XTEST_POINTER),
                    1,
                ),
                (
                    (ResourceId(0x200), crate::xinput::DEVICEID_XTEST_KEYBOARD),
                    1,
                ),
            ]),
            xi1_event_classes: HashSet::from([xi1_class_xtest_pointer, xi1_class_xtest_keyboard]),
            xi1_window_event_classes: HashMap::from([
                (ResourceId(0x100), HashSet::from([xi1_class_xtest_pointer])),
                (ResourceId(0x200), HashSet::from([xi1_class_xtest_keyboard])),
            ]),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
    state.drop_window_subscriptions(&[ResourceId(0x100)]);
    assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
    // Surviving window's subscription stays.
    assert_eq!(state.subscribers(ResourceId(0x200), 0x0040_0000).len(), 1);
    let client = state.clients.get(&1).unwrap();
    assert!(
        !client
            .xi2_masks
            .contains_key(&(ResourceId(0x100), crate::xinput::DEVICEID_XTEST_POINTER))
    );
    assert!(
        client
            .xi2_masks
            .contains_key(&(ResourceId(0x200), crate::xinput::DEVICEID_XTEST_KEYBOARD))
    );
    assert!(
        !client
            .xi1_window_event_classes
            .contains_key(&ResourceId(0x100))
    );
    assert!(
        client
            .xi1_window_event_classes
            .contains_key(&ResourceId(0x200))
    );
    assert_eq!(
        client.xi1_event_classes,
        HashSet::from([xi1_class_xtest_keyboard])
    );
}

#[test]
fn replay_pointer_delivers_to_button_press_window_not_grab_owner() {
    use std::{
        collections::HashMap as StdHashMap,
        io::{ErrorKind, Read},
        sync::Mutex as StdMutex,
    };

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    let (grab_writer_local, mut grab_reader_remote) = UnixStream::pair().expect("socketpair");
    let (target_writer_local, mut target_reader_remote) = UnixStream::pair().expect("socketpair");
    grab_reader_remote.set_nonblocking(true).unwrap();
    target_reader_remote.set_nonblocking(true).unwrap();

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        let grab_window = ResourceId(0x0010_0002);
        let target_window = ResourceId(0x0020_0002);
        s.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: grab_window,
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        s.resources.create_window(
            ClientId(2),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: target_window,
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = s.resources.map_window(grab_window);
        let _ = s.resources.map_window(target_window);
        s.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(grab_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(grab_window, 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.clients.insert(
            2,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(target_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(target_window, 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.set_pointer_grab(ActivePointerGrab {
            owner: ClientId(1),
            grab_window,
            event_mask: u16::MAX,
            cursor: ResourceId(0),
            time: 0,
            owner_events: false,
            via_xi2: false,
            implicit: false,
            passive: true,
            xi2_mask: 0,
        });
        assert_eq!(s.subscribers(grab_window, 0x0000_0004).len(), 1);
        assert_eq!(s.subscribers(target_window, 0x0000_0004).len(), 1);
        assert!(s.resources.window(target_window).is_some());
        assert!(
            s.resources
                .pointer_target_at(target_window, 10, 10)
                .is_some()
        );
        assert!(
            s.pointer_propagation_target(target_window, 10, 10, 0x0000_0004)
                .is_some()
        );
    }

    let mut map = StdHashMap::new();
    map.insert(0xCAFE_u32, ResourceId(0x0020_0002));
    let xid_map = map;

    route_button_press_no_grab(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 10,
            root_y: 10,
            event_x: 10,
            event_y: 10,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );

    let mut buf = [0u8; 32];
    let grab_read = grab_reader_remote.read(&mut buf);
    assert!(
        matches!(grab_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
        "grab owner must not receive replayed ButtonPress; got {grab_read:?}",
    );
    let target_read = target_reader_remote.read(&mut buf);
    assert!(
        matches!(target_read, Ok(32)),
        "target window subscriber should receive replayed ButtonPress; got {target_read:?}",
    );
    assert_eq!(buf[0], 4, "event type should be ButtonPress");
    assert_eq!(&buf[12..16], &0x0020_0002u32.to_le_bytes());
}

#[test]
fn passive_grab_owner_events_keeps_child_delivery_on_owned_windows() {
    use std::{
        collections::HashMap as StdHashMap,
        io::{ErrorKind, Read},
        sync::Mutex as StdMutex,
    };

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    let (grab_writer_local, mut grab_reader_remote) = UnixStream::pair().expect("socketpair");
    let (child_writer_local, mut child_reader_remote) = UnixStream::pair().expect("socketpair");
    grab_reader_remote.set_nonblocking(true).unwrap();
    child_reader_remote.set_nonblocking(true).unwrap();

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        let grab_window = ResourceId(0x0010_0002);
        let child_window = ResourceId(0x0010_0003);
        s.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: grab_window,
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        s.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: child_window,
                parent: grab_window,
                x: 10,
                y: 10,
                width: 40,
                height: 40,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = s.resources.map_window(grab_window);
        let _ = s.resources.map_window(child_window);
        s.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(grab_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(grab_window, 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.clients.insert(
            2,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(child_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(child_window, 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.set_pointer_grab(ActivePointerGrab {
            owner: ClientId(1),
            grab_window,
            event_mask: u16::MAX,
            cursor: ResourceId(0),
            time: 0,
            owner_events: true,
            via_xi2: true,
            implicit: false,
            passive: true,
            xi2_mask: u64::MAX,
        });
        s.button_grabs.push(PassiveButtonGrab {
            device_id: 0,
            owner: ClientId(1),
            grab_window,
            button: 1,
            modifiers: 0,
            owner_events: true,
            event_mask: 0xFFFF_FFFF,
            pointer_mode: 0,
            keyboard_mode: 1,
            confine_to: ResourceId(0),
            via_xi2: true,
        });
    }

    let mut map = StdHashMap::new();
    map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
    let xid_map = map;

    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 20,
            root_y: 20,
            event_x: 20,
            event_y: 20,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );

    let mut buf = [0u8; 32];
    let child_read = child_reader_remote.read(&mut buf);
    assert!(
        matches!(child_read, Ok(32)),
        "owner_events=true passive grab must still deliver to the owned child; got {child_read:?}",
    );
    assert_eq!(buf[0], 4, "event type should be ButtonPress");
    assert_eq!(&buf[12..16], &0x0010_0003u32.to_le_bytes());

    let grab_read = grab_reader_remote.read(&mut buf);
    assert!(
        matches!(grab_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
        "owner_events=true passive grab must not redirect owned-child clicks to the grab owner; got {grab_read:?}",
    );
}

#[test]
fn passive_grab_owner_events_keeps_descendant_delivery_even_when_child_owned_elsewhere() {
    use std::{
        collections::HashMap as StdHashMap,
        io::{ErrorKind, Read},
        sync::Mutex as StdMutex,
    };

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    let (grab_writer_local, mut grab_reader_remote) = UnixStream::pair().expect("socketpair");
    let (child_writer_local, mut child_reader_remote) = UnixStream::pair().expect("socketpair");
    grab_reader_remote.set_nonblocking(true).unwrap();
    child_reader_remote.set_nonblocking(true).unwrap();

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        let grab_window = ResourceId(0x0010_0010);
        let child_window = ResourceId(0x0010_0011);
        s.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: grab_window,
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        s.resources.create_window(
            ClientId(2),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: child_window,
                parent: grab_window,
                x: 10,
                y: 10,
                width: 40,
                height: 40,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = s.resources.map_window(grab_window);
        let _ = s.resources.map_window(child_window);
        s.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(grab_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(grab_window, 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.clients.insert(
            2,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(child_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(child_window, 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.set_pointer_grab(ActivePointerGrab {
            owner: ClientId(1),
            grab_window,
            event_mask: u16::MAX,
            cursor: ResourceId(0),
            time: 0,
            owner_events: true,
            via_xi2: true,
            implicit: false,
            passive: true,
            xi2_mask: u64::MAX,
        });
        s.button_grabs.push(PassiveButtonGrab {
            device_id: 0,
            owner: ClientId(1),
            grab_window,
            button: 1,
            modifiers: 0,
            owner_events: true,
            event_mask: 0xFFFF_FFFF,
            pointer_mode: 0,
            keyboard_mode: 1,
            confine_to: ResourceId(0),
            via_xi2: true,
        });
    }

    let mut map = StdHashMap::new();
    map.insert(0xCAFE_u32, ResourceId(0x0010_0010));
    let xid_map = map;

    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 20,
            root_y: 20,
            event_x: 20,
            event_y: 20,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );

    let mut buf = [0u8; 32];
    let child_read = child_reader_remote.read(&mut buf);
    assert!(
        matches!(child_read, Ok(32)),
        "owner_events=true passive grab must still deliver to the descendant child even when another client owns it; got {child_read:?}",
    );
    let grab_read = grab_reader_remote.read(&mut buf);
    assert!(
        matches!(grab_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
        "owner_events=true passive grab must not redirect descendant clicks to the grab owner; got {grab_read:?}",
    );
}

#[test]
fn pointer_event_fanout_filters_by_mask() {
    use std::{collections::HashMap as StdHashMap, sync::Mutex as StdMutex};

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    // Client A: ButtonPress on window 0x0010_0002.
    let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client B: MotionNotify on window 0x0010_0002.
    let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client C: no pointer events at all.
    let (c_writer_local, _c_reader_remote) = UnixStream::pair().expect("socketpair");

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        s.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(a_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0004)]), // ButtonPress
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.clients.insert(
            2,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(b_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0040)]), // PointerMotion
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.clients.insert(
            3,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(c_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0030_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
    }

    let mut map = StdHashMap::new();
    map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
    let xid_map = map;

    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 1,
            root_y: 2,
            event_x: 3,
            event_y: 4,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );

    let s = state.lock().unwrap();
    assert_eq!(
        s.subscribers(ResourceId(0x0010_0002), 0x0000_0004).len(),
        1,
        "only client A selected ButtonPress"
    );
    assert_eq!(
        s.subscribers(ResourceId(0x0010_0002), 0x0000_0040).len(),
        1,
        "only client B selected MotionNotify"
    );
}

#[test]
fn pointer_event_fanout_delivers_motion_under_button_motion_mask() {
    use std::{
        collections::HashMap as StdHashMap,
        io::{ErrorKind, Read},
        sync::Mutex as StdMutex,
    };

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    // Client A: subscribes to ButtonMotion (0x2000) only. Mirrors
    // wmaker's frame mask: it expects motion while a button is held.
    let (a_writer_local, mut a_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client B: no motion mask at all — must not receive anything.
    let (b_writer_local, mut b_reader_remote) = UnixStream::pair().expect("socketpair");

    a_reader_remote.set_nonblocking(true).unwrap();
    b_reader_remote.set_nonblocking(true).unwrap();

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        // Top-level window so pointer_target_at returns the same id.
        s.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(0x0010_0002),
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = s.resources.map_window(ResourceId(0x0010_0002));
        s.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(a_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_2000)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
        s.clients.insert(
            2,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(b_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
    }

    let mut map = StdHashMap::new();
    map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
    let xid_map = map;

    // Motion with button 1 held (state bit 8 == 0x100).
    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::MotionNotify,
            host_xid: 0xCAFE,
            detail: 0,
            time: 0,
            root_x: 5,
            root_y: 5,
            event_x: 5,
            event_y: 5,
            state: 0x0100,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );

    let mut buf = [0u8; 32];
    let a_read = a_reader_remote.read(&mut buf);
    assert!(
        matches!(a_read, Ok(32)),
        "client with ButtonMotion mask should receive 32-byte MotionNotify when a button is held; got {a_read:?}",
    );
    assert_eq!(buf[0], 6, "event type should be MotionNotify");

    let b_read = b_reader_remote.read(&mut buf);
    assert!(
        matches!(b_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
        "client with no motion mask must not receive motion; got {b_read:?}",
    );

    // Motion without any button held: ButtonMotion subscriber must NOT receive.
    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::MotionNotify,
            host_xid: 0xCAFE,
            detail: 0,
            time: 0,
            root_x: 5,
            root_y: 5,
            event_x: 5,
            event_y: 5,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );
    let a_read2 = a_reader_remote.read(&mut buf);
    assert!(
        matches!(a_read2, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
        "ButtonMotion-only subscriber must NOT receive motion when no button is held; got {a_read2:?}",
    );
}

#[test]
fn pointer_event_fanout_drops_unknown_host_xid() {
    use std::{collections::HashMap as StdHashMap, sync::Mutex as StdMutex};

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        s.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(Transport::Unix(a_writer_local))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0004)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: std::collections::VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
    }

    let xid_map: crate::host_x11::HostXidMap = StdHashMap::new(); // empty

    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            origin: crate::core_loop::message::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE, // not in map
            detail: 1,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        },
    );

    assert!(state.lock().unwrap().clients.contains_key(&1));
}

#[test]
fn key_grab_lookup_exact_match() {
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    let owner = ClientId(1);
    s.key_grabs.push(KeyGrab {
        device_id: 0,
        owner,
        grab_window: win,
        keycode: 24,
        modifiers: 0x0040,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
        via_xi2: false,
        xi2_mask: 0,
    });
    let hit = s.find_key_grab(
        win,
        24,
        0x0040,
        crate::xinput::DEVICEID_XTEST_KEYBOARD,
        None,
    );
    assert!(hit.is_some());
    assert_eq!(hit.unwrap().owner, owner);
}

#[test]
fn key_grab_lookup_any_modifier_wildcard() {
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    s.key_grabs.push(KeyGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: win,
        keycode: 24,
        modifiers: 0x8000,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
        via_xi2: false,
        xi2_mask: 0,
    });
    assert!(
        s.find_key_grab(
            win,
            24,
            0x0040,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
            None
        )
        .is_some()
    );
    assert!(
        s.find_key_grab(
            win,
            24,
            0x0000,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
            None
        )
        .is_some()
    );
    assert!(
        s.find_key_grab(
            win,
            25,
            0x0040,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
            None
        )
        .is_none()
    );
}

#[test]
fn key_grab_lookup_any_keycode_wildcard() {
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    s.key_grabs.push(KeyGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: win,
        keycode: 0,
        modifiers: 0x0040,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
        via_xi2: false,
        xi2_mask: 0,
    });
    assert!(
        s.find_key_grab(
            win,
            24,
            0x0040,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
            None
        )
        .is_some()
    );
    assert!(
        s.find_key_grab(
            win,
            99,
            0x0040,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
            None
        )
        .is_some()
    );
    assert!(
        s.find_key_grab(
            win,
            24,
            0x0000,
            crate::xinput::DEVICEID_XTEST_KEYBOARD,
            None
        )
        .is_none()
    );
}

#[test]
fn active_keyboard_grab_set_and_clear() {
    let mut s = ServerState::new();
    assert!(s.active_keyboard_grab.is_none());
    s.active_keyboard_grab = Some(ActiveKeyboardGrab {
        owner: ClientId(7),
        grab_window: ResourceId(0xff),
        source: ActiveKeyboardGrabSource::Explicit,
        owner_events: false,
        via_xi2: false,
        xi2_mask: 0,
    });
    assert_eq!(s.active_keyboard_grab.unwrap().owner, ClientId(7));
    s.active_keyboard_grab = None;
    assert!(s.active_keyboard_grab.is_none());
}

fn add_test_client(state: &mut ServerState, client_id: u32, base: u32) {
    state.clients.insert(
        client_id,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: base,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
}

#[test]
fn pointer_propagation_walks_parent_chain_to_root() {
    // Reproduces the desk-1 right-click bug: pointer-on-child of root,
    // child has no ButtonPress mask, root does. The event must propagate
    // up to root.
    use crate::resources::ROOT_WINDOW;
    use yserver_protocol::x11::CreateWindowRequest;

    let mut state = ServerState::new();
    add_test_client(&mut state, 1, 0x0010_0000);

    // Child of root, full screen, no ButtonPress mask (e16's "Root-bg").
    let child = ResourceId(0x0010_0004);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(child);

    // e16 selects ButtonPress on root.
    let button_press_mask: u32 = 0x0000_0004;
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, button_press_mask);

    // Click hits the child at (136, 111) — relative to child since child is
    // at (0, 0). pointer_propagation_target should walk up to root.
    let result = state.pointer_propagation_target(child, 136, 111, button_press_mask);
    assert!(result.is_some(), "expected propagation to root");
    let (window, x, y, subs) = result.unwrap();
    assert_eq!(window, ROOT_WINDOW);
    // Child is at (0, 0) on root, so coords are unchanged.
    assert_eq!(x, 136);
    assert_eq!(y, 111);
    assert_eq!(subs.len(), 1);
}

#[test]
fn pointer_propagation_translates_offset_coords() {
    // Click at (10, 20) inside a child positioned at (50, 60) on root —
    // should translate to (60, 80) when delivered to root.
    use crate::resources::ROOT_WINDOW;
    use yserver_protocol::x11::CreateWindowRequest;

    let mut state = ServerState::new();
    add_test_client(&mut state, 1, 0x0010_0000);

    let child = ResourceId(0x0010_0010);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child,
            parent: ROOT_WINDOW,
            x: 50,
            y: 60,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(child);

    let button_press_mask: u32 = 0x0000_0004;
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, button_press_mask);

    let (window, x, y, _) = state
        .pointer_propagation_target(child, 10, 20, button_press_mask)
        .expect("propagation should find root");
    assert_eq!(window, ROOT_WINDOW);
    assert_eq!(x, 60);
    assert_eq!(y, 80);
}

#[test]
fn pointer_propagation_stops_at_first_subscriber() {
    // Both child and root subscribe; event delivered to child (first hit).
    use crate::resources::ROOT_WINDOW;
    use yserver_protocol::x11::CreateWindowRequest;

    let mut state = ServerState::new();
    add_test_client(&mut state, 1, 0x0010_0000);

    let child = ResourceId(0x0010_0020);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child,
            parent: ROOT_WINDOW,
            x: 5,
            y: 5,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(child);

    let button_press_mask: u32 = 0x0000_0004;
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(child, button_press_mask);
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, button_press_mask);

    let (window, x, y, _) = state
        .pointer_propagation_target(child, 30, 40, button_press_mask)
        .expect("propagation should hit child first");
    assert_eq!(window, child);
    assert_eq!(x, 30);
    assert_eq!(y, 40);
}

#[test]
fn pointer_propagation_returns_none_when_nothing_subscribes() {
    use crate::resources::ROOT_WINDOW;
    use yserver_protocol::x11::CreateWindowRequest;

    let mut state = ServerState::new();
    add_test_client(&mut state, 1, 0x0010_0000);

    let child = ResourceId(0x0010_0030);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(child);

    let button_press_mask: u32 = 0x0000_0004;
    let result = state.pointer_propagation_target(child, 10, 10, button_press_mask);
    assert!(result.is_none());
}

#[test]
fn cow_with_empty_input_shape_passes_clicks_to_sibling_below() {
    use crate::resources::{ROOT_VISUAL, ROOT_WINDOW};
    use yserver_protocol::x11::CreateWindowRequest;

    let mut state = ServerState::new();

    // Non-COW sibling at (0,0) 800x600, default (full) input shape.
    let sib = ResourceId(0x0010_0080);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: sib,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(sib);

    // Materialize the full-screen COW; the compositor empties its
    // input region.
    let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
    state.resources.materialize_cow_resource(host_xid);
    state
        .shape_windows
        .entry(crate::resources::COMPOSITE_OVERLAY_WINDOW)
        .or_default()
        .input = Some(Vec::new());

    // Click at (50, 50): inside both sibling and COW geometry. COW's
    // empty input shape → hit_test_child(COW) = None → iteration
    // falls through to `sib`.
    let (target, _, _) = state
        .root_pointer_target_at(50, 50)
        .expect("trace hits sibling below COW");
    assert_eq!(
        target, sib,
        "empty COW input shape must let clicks through to sibling below"
    );
}

// ---- #133 step 8 (P9): border-inclusive input ----------------
//
// The `ServerState` half of the hit test. `resources.rs` pins the
// `ResourceTable` mirror and `child_containing_point` on the same
// fixture; these add the input-shape gate (which lives only here)
// and cross-check the two implementations against each other.

/// Same fixture as `resources::tests::bordered_frame_table`: a root
/// child at (100, 200), 300x400, `border_width = 16`. Content origin
/// (116, 216); outer box x [100, 432) x y [200, 632).
fn bordered_frame_state() -> (ServerState, ResourceId) {
    use crate::resources::ROOT_VISUAL;
    use yserver_protocol::x11::CreateWindowRequest;

    let mut state = ServerState::new();
    let frame = ResourceId(0x0010_0100);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: frame,
            parent: crate::resources::ROOT_WINDOW,
            x: 100,
            y: 200,
            width: 300,
            height: 400,
            border_width: 16,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(frame);
    (state, frame)
}

/// (name, root_x, root_y, want content x, want content y).
const BORDER_PROBES: &[(&str, i16, i16, i16, i16)] = &[
    ("left edge", 100, 416, -16, 200),
    ("top edge", 266, 200, 150, -16),
    ("right edge", 431, 416, 315, 200),
    ("bottom edge", 266, 631, 150, 415),
    ("top-left corner", 100, 200, -16, -16),
    ("top-right corner", 431, 200, 315, -16),
    ("bottom-left corner", 100, 631, -16, 415),
    ("bottom-right corner", 431, 631, 315, 415),
];

#[test]
fn server_hit_test_border_sides_and_corners_report_content_coords() {
    let (state, frame) = bordered_frame_state();
    for &(name, ax, ay, wx, wy) in BORDER_PROBES {
        assert_eq!(
            state.root_pointer_target_at(ax, ay),
            Some((frame, wx, wy)),
            "{name}: root ({ax},{ay}) must hit the frame at content ({wx},{wy})"
        );
    }
}

#[test]
fn server_hit_test_rejects_one_pixel_outside_the_border() {
    let (state, _frame) = bordered_frame_state();
    for (ax, ay) in [(99i16, 416i16), (266, 199), (432, 416), (266, 632)] {
        assert_eq!(
            state.root_pointer_target_at(ax, ay),
            Some((crate::resources::ROOT_WINDOW, ax, ay)),
            "({ax},{ay}) is outside the outer box and must not hit"
        );
    }
}

/// The two implementations (8.4). `ServerState::hit_test_child` and
/// `ResourceTable::pointer_target_at_inner` are separate copies of
/// the same rule, and divergence between an authoritative tree and
/// its mirror is a standing bug class here — so drive both for the
/// same points and compare, coordinates included. With no input
/// shape set the shape gate is a pass-through, which is what makes
/// the two comparable at all.
#[test]
fn hit_test_implementations_agree_on_border_points() {
    let (state, frame) = bordered_frame_state();
    for &(name, ax, ay, wx, wy) in BORDER_PROBES {
        let via_server = state.root_pointer_target_at(ax, ay);
        let via_table = state
            .resources
            .pointer_target_at(crate::resources::ROOT_WINDOW, ax, ay);
        assert_eq!(
            via_server, via_table,
            "{name}: ServerState and ResourceTable hit tests diverge"
        );
        assert_eq!(via_server, Some((frame, wx, wy)), "{name}");
        // The third implementation, which returns only the window.
        assert_eq!(
            state.resources.child_containing_point(
                crate::resources::ROOT_WINDOW,
                i32::from(ax),
                i32::from(ay)
            ),
            Some(frame),
            "{name}: child_containing_point diverges"
        );
    }
}

/// 8.3 — the input shape is applied in CONTENT coordinates
/// (`dix/window.c:2995` passes `x - pWin->drawable.x` to
/// `RegionContainsPoint(wInputShape(pWin), ...)`), and it is a THIRD
/// shape, distinct from bounding and clip.
///
/// A shape rect covering only the top-left 10x10 of the CONTENT must
/// therefore accept content (0,0) and reject the border ring, whose
/// content coordinates are negative — and, crucially, must reject
/// content (16,16), which is what the pre-fix arithmetic handed the
/// shape test when the pointer was at the content origin.
#[test]
fn input_shape_is_tested_in_content_coordinates() {
    use yserver_protocol::x11::xfixes;

    let (mut state, frame) = bordered_frame_state();
    state.shape_windows.entry(frame).or_default().input = Some(vec![xfixes::RegionRect {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    }]);

    // Content origin (116, 216) → content (0,0): inside the shape.
    assert_eq!(
        state.root_pointer_target_at(116, 216),
        Some((frame, 0, 0)),
        "the content origin is inside a shape rooted at content (0,0)"
    );
    // Content (9,9): still inside.
    assert_eq!(state.root_pointer_target_at(125, 225), Some((frame, 9, 9)));
    // Content (10,10): outside the shape → falls through to root,
    // even though it is well inside the window's outer box.
    assert_eq!(
        state.root_pointer_target_at(126, 226),
        Some((crate::resources::ROOT_WINDOW, 126, 226))
    );
    // The border ring is inside `borderClip` but outside the input
    // shape (its content coords are negative) → no hit.
    assert_eq!(
        state.root_pointer_target_at(100, 200),
        Some((crate::resources::ROOT_WINDOW, 100, 200)),
        "the top-left border corner is outside a content-space shape"
    );
    // The outer origin (116-16, 216-16) is what the OLD code called
    // (0,0). Pinning it as a miss is what makes this a regression
    // test rather than a restatement.
    assert_eq!(
        state.root_pointer_target_at(100 + 16 + 10, 200 + 16 + 10),
        Some((crate::resources::ROOT_WINDOW, 126, 226))
    );
}

/// The propagation walk climbs the ancestry when the hit window has
/// no subscriber, translating the coordinate to whichever window it
/// stops at. That walk must be the exact inverse of the hit-test
/// descent — border term included — or the coordinate drifts by one
/// border width per level crossed.
#[test]
fn propagation_walk_up_undoes_the_border_term() {
    use crate::resources::{ROOT_VISUAL, ROOT_WINDOW};
    use yserver_protocol::x11::CreateWindowRequest;

    let (mut state, frame) = bordered_frame_state();
    let child = ResourceId(0x0010_0101);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child,
            parent: frame,
            x: 10,
            y: 20,
            width: 100,
            height: 100,
            border_width: 4,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(child);

    // Root (130, 240) is the child's own content origin.
    let (hit, hx, hy) = state.root_pointer_target_at(130, 240).expect("child hit");
    assert_eq!((hit, hx, hy), (child, 0, 0));

    // Nobody subscribes anywhere, so the walk runs to the root and
    // returns None — but it must arrive at root coordinates that
    // still name the same physical pixel. Assert the arithmetic
    // directly via the inverse helpers, level by level.
    let cw = state.resources.window(child).expect("child");
    let fw = state.resources.window(frame).expect("frame");
    let (fx, fy) = cw.to_parent_coords(hx, hy);
    assert_eq!((fx, fy), (14, 24), "child content → frame content");
    assert_eq!(
        fw.to_parent_coords(fx, fy),
        (130, 240),
        "frame content → root content == the original root point"
    );

    // And end to end through the real walk: subscribe to
    // ButtonPress on the ROOT window only, so the walk has to climb
    // both levels, and check the coordinate it reports.
    const BUTTON_PRESS: u32 = 0x0000_0004;
    state.clients.insert(
        7,
        ClientState {
            writer: make_test_writer(),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ROOT_WINDOW, BUTTON_PRESS)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    let (win, px, py, _subs) = state
        .pointer_propagation_target(hit, hx, hy, BUTTON_PRESS)
        .expect("propagates to root");
    assert_eq!(win, ROOT_WINDOW);
    assert_eq!(
        (px, py),
        (130, 240),
        "the coordinate reported on root must be the original root point"
    );
}

#[test]
fn cow_with_non_empty_input_shape_descends_into_stage() {
    use crate::resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_VISUAL};
    use yserver_protocol::x11::{CreateWindowRequest, xfixes};

    let mut state = ServerState::new();

    let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
    state.resources.materialize_cow_resource(host_xid);
    // Compositor populates COW input shape covering the stage region.
    state
        .shape_windows
        .entry(COMPOSITE_OVERLAY_WINDOW)
        .or_default()
        .input = Some(vec![xfixes::RegionRect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    }]);

    let stage = ResourceId(0x0010_0050);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: stage,
            parent: COMPOSITE_OVERLAY_WINDOW,
            x: 10,
            y: 10,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(stage);

    let (target, _, _) = state.root_pointer_target_at(50, 50).expect("hit");
    assert_eq!(
        target, stage,
        "non-empty COW input shape lets the trace descend to stage"
    );
}

// ── DRIFT 1 characterization net: input-shape empty-vs-absent ──
//
// These pin the CORE resource tree's empty-vs-absent input-shape
// semantics, which are the source of truth the KMS v2 backend must
// converge onto (see
// docs/superpowers/findings/2026-06-18-pointer-stacking-dual-authority-diagnosis.md,
// DRIFT 1). They must stay green through the backend-demotion work;
// a change here is a real regression of the truth source, not an
// intended effect of the refactor.

/// Helper: a full-screen mapped top-level child of root.
#[cfg(test)]
fn fullscreen_child(state: &mut ServerState, id: u32) -> ResourceId {
    use crate::resources::{ROOT_VISUAL, ROOT_WINDOW};
    use yserver_protocol::x11::CreateWindowRequest;
    let win = ResourceId(id);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: win,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(win);
    win
}

#[test]
fn core_empty_input_shape_makes_window_click_through() {
    let mut state = ServerState::new();
    // `below` created first → lower in the stack; `above` created
    // second → topmost. Both cover (50,50).
    let below = fullscreen_child(&mut state, 0x0010_0080);
    let above = fullscreen_child(&mut state, 0x0010_0090);

    // `above` gets an EMPTY (but present) input shape → click-through.
    state.shape_windows.entry(above).or_default().input = Some(vec![]);

    let (target, _, _) = state
        .root_pointer_target_at(50, 50)
        .expect("trace resolves a window");
    assert_eq!(
        target, below,
        "empty (Some([])) input shape must be click-through, so the \
             topmost window is skipped and the click lands on the window below"
    );
}

#[test]
fn core_full_input_shape_makes_window_opaque() {
    use yserver_protocol::x11::xfixes;

    let mut state = ServerState::new();
    let below = fullscreen_child(&mut state, 0x0010_0080);
    let above = fullscreen_child(&mut state, 0x0010_0090);

    // Contrast with the empty case: a full-coverage input shape is
    // opaque, so the topmost window swallows the click. This also
    // proves the empty-case test above is not vacuous.
    state.shape_windows.entry(above).or_default().input = Some(vec![xfixes::RegionRect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    }]);

    let (target, _, _) = state
        .root_pointer_target_at(50, 50)
        .expect("trace resolves a window");
    let _ = below;
    assert_eq!(
        target, above,
        "a full input shape is opaque: the topmost window receives the click"
    );
}

#[test]
fn core_absent_input_shape_is_opaque() {
    let mut state = ServerState::new();
    let below = fullscreen_child(&mut state, 0x0010_0080);
    let above = fullscreen_child(&mut state, 0x0010_0090);

    // No shape_windows entry at all (absent / None) → opaque, same
    // as a full shape. This is the case that DIFFERS from `Some([])`
    // and that the backend currently cannot distinguish (DRIFT 1).
    let (target, _, _) = state
        .root_pointer_target_at(50, 50)
        .expect("trace resolves a window");
    let _ = below;
    assert_eq!(
        target, above,
        "absent input shape (None) is opaque, NOT click-through"
    );
}

#[test]
fn dpms_transition_deadline_picks_smallest_non_zero_above_current() {
    use std::time::{Duration, Instant};
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let baseline = Instant::now();
    state.dpms.last_activity = baseline;
    state.dpms.standby_ms = 300_000;
    state.dpms.suspend_ms = 600_000;
    state.dpms.off_ms = 900_000;

    state.dpms.power_level = 0; // On
    assert_eq!(
        state.dpms_transition_deadline(),
        Some(baseline + Duration::from_millis(300_000))
    );

    state.dpms.power_level = 1; // Standby
    assert_eq!(
        state.dpms_transition_deadline(),
        Some(baseline + Duration::from_millis(600_000))
    );

    state.dpms.power_level = 2; // Suspend
    assert_eq!(
        state.dpms_transition_deadline(),
        Some(baseline + Duration::from_millis(900_000))
    );

    state.dpms.power_level = 3; // Off — nothing above
    assert_eq!(state.dpms_transition_deadline(), None);
}

#[test]
fn dpms_transition_deadline_returns_none_when_disabled() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = false; // disabled
    state.dpms.standby_ms = 300_000;
    assert!(state.dpms_transition_deadline().is_none());
}

#[test]
fn dpms_transition_deadline_returns_none_when_not_kms_capable() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = false;
    state.dpms.enabled = true; // a ynest client called DPMSEnable
    state.dpms.standby_ms = 300_000;
    // No backend to drive — no deadline.
    assert!(state.dpms_transition_deadline().is_none());
}

#[test]
fn dpms_transition_deadline_zero_skips_not_halts() {
    use std::time::{Duration, Instant};
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let baseline = Instant::now();
    state.dpms.last_activity = baseline;
    // Standby + Off disabled, Suspend at 900s.
    state.dpms.standby_ms = 0;
    state.dpms.suspend_ms = 900_000;
    state.dpms.off_ms = 0;

    state.dpms.power_level = 0; // On
    assert_eq!(
        state.dpms_transition_deadline(),
        Some(baseline + Duration::from_millis(900_000))
    );

    state.dpms.power_level = 2; // Suspend — nothing above non-zero
    assert!(state.dpms_transition_deadline().is_none());
}

#[test]
fn next_dpms_level_leapfrogs_on_equal_timeouts() {
    let mut state = ServerState::new();
    state.dpms.standby_ms = 600_000;
    state.dpms.suspend_ms = 600_000;
    state.dpms.off_ms = 600_000;
    // From On, with idle = exactly 600_000ms, highest expired wins → Off.
    assert_eq!(next_dpms_level(0, 600_000, &state.dpms), 3);
}

#[test]
fn next_dpms_level_skips_zero_levels() {
    let mut state = ServerState::new();
    state.dpms.standby_ms = 0;
    state.dpms.suspend_ms = 900_000;
    state.dpms.off_ms = 0;
    // From On, idle = 900s → Suspend (Standby and Off skipped).
    assert_eq!(next_dpms_level(0, 900_000, &state.dpms), 2);
}

#[test]
fn next_dpms_level_stable_when_under_threshold() {
    let mut state = ServerState::new();
    state.dpms.standby_ms = 300_000;
    state.dpms.suspend_ms = 600_000;
    state.dpms.off_ms = 900_000;
    assert_eq!(next_dpms_level(0, 0, &state.dpms), 0);
    assert_eq!(next_dpms_level(1, 100_000, &state.dpms), 1);
    // Already at Off (max level): cascade has nowhere to go.
    assert_eq!(next_dpms_level(3, 999_999_999, &state.dpms), 3);
}

#[test]
fn screensaver_idle_deadline_none_when_timeout_zero() {
    let mut state = ServerState::new();
    state.screensaver.timeout_ms = 0;
    assert!(state.screensaver_idle_deadline().is_none());
}

#[test]
fn screensaver_idle_deadline_none_when_suspended() {
    let mut state = ServerState::new();
    state.screensaver.timeout_ms = 60_000;
    state.screensaver.suspend_counts.insert(ClientId(7), 1);
    assert!(state.screensaver_idle_deadline().is_none());
}

#[test]
fn dpms_transition_deadline_none_when_screensaver_suspended() {
    // Xorg WaitFor.c:519 — one timer drives BOTH SS and DPMS, and
    // it isn't armed when screenSaverSuspended. XScreenSaverSuspend
    // therefore inhibits DPMS firing, which mpv/Firefox rely on.
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    state.dpms.standby_ms = 300_000;
    state.screensaver.suspend_counts.insert(ClientId(99), 1);
    assert!(state.dpms_transition_deadline().is_none());
}

#[test]
fn screensaver_idle_deadline_none_when_active() {
    let mut state = ServerState::new();
    state.screensaver.timeout_ms = 60_000;
    state.screensaver.active = ScreenSaverActive::On;
    assert!(state.screensaver_idle_deadline().is_none());
}

#[test]
fn screensaver_idle_deadline_none_when_dpms_blanked() {
    // Xorg WaitFor.c:457 — when DPMS already blanked the panel
    // the SS idle timer is suppressed (DPMS→SS coupling will
    // have already activated SS on the DPMS transition).
    let mut state = ServerState::new();
    state.screensaver.timeout_ms = 60_000;
    state.dpms.power_level = 1;
    assert!(state.screensaver_idle_deadline().is_none());
}

#[test]
fn screensaver_idle_deadline_returns_last_activity_plus_timeout() {
    use std::time::{Duration, Instant};
    let mut state = ServerState::new();
    let baseline = Instant::now();
    state.dpms.last_activity = baseline;
    state.screensaver.timeout_ms = 60_000;
    assert_eq!(
        state.screensaver_idle_deadline(),
        Some(baseline + Duration::from_millis(60_000))
    );
}

#[test]
fn screensaver_cycle_deadline_none_when_off() {
    let state = ServerState::new();
    assert!(state.screensaver_cycle_deadline().is_none());
}

#[test]
fn screensaver_cycle_deadline_some_when_on() {
    use std::time::{Duration, Instant};
    let mut state = ServerState::new();
    state.screensaver.active = ScreenSaverActive::On;
    state.screensaver.interval_ms = 600_000;
    let fire_at = Instant::now() + Duration::from_millis(600_000);
    state.screensaver.next_cycle = Some(fire_at);
    assert_eq!(state.screensaver_cycle_deadline(), Some(fire_at));
}

#[test]
fn screensaver_cycle_deadline_propagates_next_cycle_none() {
    // The invariant "interval_ms == 0 ⇒ next_cycle is None" lives
    // in the activation transition (Task 3); here we only verify
    // the deadline helper propagates a None `next_cycle` through.
    let mut state = ServerState::new();
    state.screensaver.active = ScreenSaverActive::On;
    state.screensaver.interval_ms = 0;
    state.screensaver.next_cycle = None;
    assert!(state.screensaver_cycle_deadline().is_none());
}

#[test]
fn idletime_baseline_global_returns_dpms_last_activity() {
    use std::time::Instant;
    let mut state = ServerState::new();
    let baseline = Instant::now();
    state.dpms.last_activity = baseline;
    assert_eq!(
        state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_COUNTER),
        baseline
    );
}

#[test]
fn idletime_baseline_per_device_uses_per_device_entry() {
    use std::time::{Duration, Instant};
    let mut state = ServerState::new();
    let global = Instant::now() - Duration::from_secs(60);
    let pointer = Instant::now() - Duration::from_secs(5);
    state.dpms.last_activity = global;
    state.per_device_last_activity.insert(2, pointer);
    assert_eq!(
        state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_DEVICE_VCP),
        pointer
    );
    // VCK has no per-device entry; falls back to global.
    assert_eq!(
        state.idletime_baseline(yserver_protocol::x11::sync::IDLETIME_DEVICE_VCK),
        global
    );
}

#[test]
fn idletime_baseline_unknown_counter_falls_back_to_global() {
    let state = ServerState::new();
    let baseline = state.dpms.last_activity;
    assert_eq!(state.idletime_baseline(0xdead_beef), baseline);
}

#[test]
fn idletime_alarm_deadline_none_when_no_alarms() {
    let state = ServerState::new();
    assert!(state.idletime_alarm_deadline().is_none());
}

#[test]
fn idletime_alarm_deadline_picks_smallest_active_pos_alarm() {
    use std::time::Duration;
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    let baseline = std::time::Instant::now();
    state.dpms.last_activity = baseline;

    for (id, wait) in &[(1u32, 60_000i64), (2, 30_000), (3, 90_000)] {
        state.sync_alarms.insert(
            *id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: *wait,
                delta: 0,
                test_type: x11sync::TEST_POSITIVE_TRANSITION,
                events: true,
                state: x11sync::ALARM_STATE_ACTIVE,
                event_clients: Vec::new(),
                value_type: 0,
                raw_wait: *wait,
                check_type: x11sync::TEST_POSITIVE_TRANSITION,
            },
        );
    }

    let deadline = state.idletime_alarm_deadline().expect("Some");
    let expected = baseline + Duration::from_millis(30_000);
    // Allow ±1ms for monotonic-clock resolution.
    let diff = if deadline > expected {
        deadline - expected
    } else {
        expected - deadline
    };
    assert!(
        diff < Duration::from_millis(2),
        "deadline ~ baseline + 30_000ms; got diff {diff:?}"
    );
}

#[test]
fn idletime_alarm_deadline_ignores_negative_alarms() {
    // Negative-* alarms only fire on input wake, not on a positive
    // deadline. They must not be considered when computing the
    // poll-deadline `.min()`.
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    state.sync_alarms.insert(
        1,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_NEGATIVE_TRANSITION,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_NEGATIVE_TRANSITION,
        },
    );
    assert!(state.idletime_alarm_deadline().is_none());
}

#[test]
fn idletime_alarm_deadline_ignores_inactive_alarms() {
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    state.sync_alarms.insert(
        1,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION,
            events: true,
            state: x11sync::ALARM_STATE_INACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_POSITIVE_TRANSITION,
        },
    );
    assert!(state.idletime_alarm_deadline().is_none());
}

#[test]
fn idletime_alarm_deadline_ignores_quiescent_alarm_whose_threshold_already_passed() {
    // Regression for the quiescent-state skip: a PositiveTransition +
    // delta=0 alarm that has already fired stays Active but is
    // quiescent — it doesn't re-fire until the counter drops below
    // wait_value and crosses back up (which requires input). Such an
    // alarm must NOT contribute a past-instant to the poll-deadline
    // (which would spin the poll loop with Duration::ZERO).
    use std::time::Duration;
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    // Already idle for 90s; alarm threshold is 60s — quiescent.
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
    state.sync_alarms.insert(
        1,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_POSITIVE_TRANSITION,
        },
    );
    assert!(
        state.idletime_alarm_deadline().is_none(),
        "quiescent alarm (current_idle >= wait_value) must not contribute a deadline"
    );
}

#[test]
fn idletime_alarm_deadline_none_when_screensaver_suspended() {
    // Mirrors the dpms_transition_deadline suspend gate. XScreen-
    // SaverSuspend inhibits both the DPMS cascade AND IDLETIME
    // alarms so fullscreen video (Firefox / mpv / vlc) doesn't
    // blank the screen.
    use yserver_protocol::x11::sync as x11sync;
    let mut state = ServerState::new();
    state.screensaver.suspend_counts.insert(ClientId(99), 1);
    state.sync_alarms.insert(
        1,
        crate::server::SyncAlarm {
            owner: ClientId(1),
            counter: x11sync::IDLETIME_COUNTER,
            wait_value: 60_000,
            delta: 0,
            test_type: x11sync::TEST_POSITIVE_TRANSITION,
            events: true,
            state: x11sync::ALARM_STATE_ACTIVE,
            event_clients: Vec::new(),
            value_type: 0,
            raw_wait: 60_000,
            check_type: x11sync::TEST_POSITIVE_TRANSITION,
        },
    );
    assert!(state.idletime_alarm_deadline().is_none());
}

/// XC-MISC guard test: every client-XID namespace must register in
/// xid_occupied. Seeds ONE resource per namespace at a distinct id.
/// A future XID-keyed map added without xid_occupied coverage should
/// be caught by review against this pattern (spec
/// 2026-06-12-xcmisc-design.md "maintenance hazard").
#[test]
fn xid_occupied_covers_every_namespace() {
    use crate::{
        backend::{GlyphSetHandle, PictureHandle},
        resources::{GlyphSetState, PictureKind, PictureState, ROOT_VISUAL},
    };
    use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest};

    let mut state = ServerState::new();
    let owner = ClientId(1);
    let base = 0x0010_0000u32;
    let mut expect = Vec::new();

    // ── 9 ResourceTable namespaces ──

    // 1. window
    let id_window = base + 1;
    state.resources.create_window(
        owner,
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(id_window),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    expect.push(id_window);

    // 2. pixmap
    let id_pixmap = base + 2;
    state.resources.create_pixmap(
        owner,
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(id_pixmap),
            drawable: ROOT_WINDOW,
            width: 1,
            height: 1,
        },
    );
    expect.push(id_pixmap);

    // 3. gc
    let id_gc = base + 3;
    state.resources.seed_gc_for_test(owner, ResourceId(id_gc));
    expect.push(id_gc);

    // 4. font
    let id_font = base + 4;
    state
        .resources
        .seed_font_for_test(owner, ResourceId(id_font));
    expect.push(id_font);

    // 5. cursor
    let id_cursor = base + 5;
    state.resources.create_cursor(owner, ResourceId(id_cursor));
    expect.push(id_cursor);

    // 6. colormap
    let id_colormap = base + 6;
    state
        .resources
        .create_colormap(owner, ResourceId(id_colormap), ROOT_VISUAL);
    expect.push(id_colormap);

    // 7. picture (pub map — insert literal directly)
    let id_picture = base + 7;
    state.resources.pictures.insert(
        id_picture,
        PictureState {
            client: owner,
            host_picture_xid: Some(PictureHandle::from_raw_for_test(1)),
            host_owned_pixmap: None,
            kind: PictureKind::Sourceless,
            drawable: None,
            window: None,
        },
    );
    expect.push(id_picture);

    // 8. glyphset (pub map — insert literal directly)
    let id_glyphset = base + 8;
    state.resources.glyphsets.insert(
        id_glyphset,
        GlyphSetState {
            client: owner,
            host_glyphset_xid: GlyphSetHandle::from_raw_for_test(1),
        },
    );
    expect.push(id_glyphset);

    // 9. DRI3 syncobj (the kernel object itself is backend-owned; this
    // is the ordinary X resource record).
    let id_dri3_syncobj = base + 9;
    assert!(
        state
            .resources
            .register_dri3_syncobj(ResourceId(id_dri3_syncobj), owner)
    );
    expect.push(id_dri3_syncobj);

    // ── 11 ServerState extension namespaces ──

    // 9. xfixes_regions
    let id_xfixes = base + 10;
    state.xfixes_regions.insert(
        id_xfixes,
        XFixesRegion {
            owner,
            rects: vec![],
        },
    );
    expect.push(id_xfixes);

    // 10. pointer_barriers
    let id_barrier = base + 11;
    state.pointer_barriers.insert(
        id_barrier,
        PointerBarrier {
            owner,
            window: ROOT_WINDOW,
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 10,
            directions: 0,
            devices: Vec::new(),
            hit: false,
            seen: false,
            event_id: 1,
            release_event_id: 0,
            last_timestamp: 0,
        },
    );
    expect.push(id_barrier);

    // 11. sync_counters
    let id_sync_counter = base + 12;
    state
        .sync_counters
        .insert(id_sync_counter, SyncCounter { owner, value: 0 });
    expect.push(id_sync_counter);

    // 12. sync_alarms
    let id_sync_alarm = base + 13;
    state.sync_alarms.insert(
        id_sync_alarm,
        SyncAlarm {
            owner,
            ..SyncAlarm::default()
        },
    );
    expect.push(id_sync_alarm);

    // 13. sync_fences
    let id_sync_fence = base + 14;
    state.sync_fences.insert(
        id_sync_fence,
        SyncFence {
            owner,
            triggered: false,
        },
    );
    expect.push(id_sync_fence);

    // 14. damage_objects
    let id_damage = base + 15;
    state.damage_objects.insert(
        id_damage,
        DamageObject {
            owner,
            drawable: ROOT_WINDOW,
            level: 0,
            rects: vec![],
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );
    expect.push(id_damage);

    // 15. mit_shm_segments — requires a real fd; use memfd_create
    let id_shm = base + 16;
    let fd = unsafe { libc::memfd_create(c"xcmisc-test-shm".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    let rc = unsafe { libc::ftruncate(fd, 4096) };
    assert_eq!(rc, 0, "ftruncate failed");
    let shm_seg = MitShmSegment::from_fd(owner, fd, false).expect("MitShmSegment::from_fd");
    state.mit_shm_segments.insert(id_shm, shm_seg);
    expect.push(id_shm);

    // 16. glx_contexts
    let id_glx_ctx = base + 17;
    state.glx_contexts.insert(
        id_glx_ctx,
        GlxContext {
            owner,
            screen: 0,
            visual_id: 0,
            fbconfig: 0,
            render_type: 0,
            share_list: 0,
            is_direct: true,
        },
    );
    expect.push(id_glx_ctx);

    // 17. glx_drawables
    let id_glx_draw = base + 18;
    state.glx_drawables.insert(
        id_glx_draw,
        GlxDrawable {
            owner,
            kind: GlxDrawableKind::Window,
            x_drawable: 0,
            fbconfig: 0,
            width: 0,
            height: 0,
            event_mask: 0,
            glx_export_host_xid: None,
            texture_target: yserver_protocol::x11::glx::GLX_TEXTURE_2D_EXT,
        },
    );
    expect.push(id_glx_draw);

    // 18. present_event_selections
    let id_present = base + 19;
    state.present_event_selections.insert(
        id_present,
        PresentEventSelection {
            owner,
            window: ROOT_WINDOW,
            event_mask: 0,
        },
    );
    expect.push(id_present);

    // 19. record contexts
    let id_record = base + 20;
    state.record.insert_for_test(id_record, owner);
    expect.push(id_record);

    // ── assertions ──

    for id in &expect {
        assert!(state.xid_occupied(*id), "id 0x{id:x} must be occupied");
    }
    assert_eq!(
        expect.len(),
        20,
        "one seed per namespace — update when adding namespaces"
    );
    assert!(!state.xid_occupied(base + 100), "unseeded id must be free");

    // used_xids_in returns exactly the seeded set, sorted
    let used = state.used_xids_in(base, 0x000F_FFFF);
    let mut sorted = expect.clone();
    sorted.sort_unstable();
    assert_eq!(used, sorted);
    // out-of-range base sees none of them
    assert!(state.used_xids_in(0x0020_0000, 0x000F_FFFF).is_empty());
}
