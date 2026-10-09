mod composite;
mod core_misc;
mod damage_ext;
mod drawing;
mod dri3_shm;
mod gc_pixmap;
mod glx;
mod input_control;
mod present_pixmap;
mod present_supersede;
mod randr_crtc;
mod randr_output;
mod render;
mod saver_dpms;
mod selection;
mod sync;
mod vidmode;
mod window;
mod xfixes_shape;
mod xi1;
mod xi2;
mod xi2_allow;
mod xi2_grabs;
mod xi_config;
mod xi_hotplug;
mod xkb;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, atomic::AtomicU16},
};

use yserver_protocol::x11::{
    ClientByteOrder, ClientId, CreateGcRequest, CreatePixmapRequest, CreateWindowRequest,
    RequestHeader, ResourceId, SequenceNumber, screensaver as x11screensaver, sync as x11sync,
};

use super::*;
use crate::{
    backend::{
        FillStyle,
        recording::{RecordedCall, RecordingBackend},
    },
    resources::ROOT_WINDOW,
    server::{ClientState, ScreenSaverActive, ServerState},
};

fn seed_present_clock(state: &mut ServerState, msc: u64, ust: u64) {
    seed_present_domain_clock(state, 0, 0, msc, ust);
}

fn seed_present_domain_clock(
    state: &mut ServerState,
    crtc_id: u32,
    crtc_epoch: u64,
    msc: u64,
    ust: u64,
) {
    let completion = crate::backend::PresentClockSample {
        msc,
        ust,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    state.present_crtc_clocks.insert(
        (crtc_id, crtc_epoch),
        crate::server::PresentCrtcClock {
            epoch: crtc_epoch,
            msc,
            ust,
            completion,
        },
    );
}

fn create_present_test_window(
    state: &mut ServerState,
    xid: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) {
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(xid),
            parent: ROOT_WINDOW,
            x,
            y,
            width,
            height,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
}

fn install_client_with_transport(
    state: &mut ServerState,
    id: u32,
    transport: crate::transport::Transport,
) {
    state.clients.insert(
        id,
        ClientState {
            writer: Arc::new(Mutex::new(transport)),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            // Permissive resource-id range so tests driving
            // `process_request` can use arbitrary non-zero
            // resource xids without tripping
            // `xid_out_of_client_range`. Real clients get a
            // tight base/mask from the connection handshake;
            // this is the test-fixture analogue of "every xid
            // is in range".
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
}

fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
    let (writer, peer) = UnixStream::pair().unwrap();
    install_client_with_transport(state, id, crate::transport::Transport::Unix(writer));
    peer
}

fn install_capture_client(state: &mut ServerState, id: u32) -> crate::transport::CapturedPeer {
    let (transport, peer) = crate::transport::Transport::capture_pair();
    install_client_with_transport(state, id, transport);
    peer
}

trait TestPeer: Read {
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()>;
}

impl TestPeer for UnixStream {
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        UnixStream::set_nonblocking(self, nonblocking)
    }
}

impl TestPeer for crate::transport::CapturedPeer {
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        crate::transport::CapturedPeer::set_nonblocking(self, nonblocking)
    }
}

fn set_test_pointer_grab(
    state: &mut ServerState,
    owner: u32,
    window: u32,
    passive: bool,
    via_xi2: bool,
) {
    state.set_pointer_grab(crate::server::ActivePointerGrab {
        owner: ClientId(owner),
        grab_window: ResourceId(window),
        event_mask: u16::MAX,
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2,
        implicit: false,
        passive,
        xi2_mask: if via_xi2 { u64::MAX } else { 0 },
    });
}

fn read_all_available(peer: &mut impl TestPeer) -> Vec<u8> {
    peer.set_nonblocking(true).expect("set_nonblocking");
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match peer.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("read failed: {err}"),
        }
    }
    peer.set_nonblocking(false).expect("unset_nonblocking");
    out
}

fn read_all_or_buffered(state: &mut ServerState, client_id: u32, peer: &mut UnixStream) -> Vec<u8> {
    let mut out = read_all_available(peer);
    let client = state.clients.get_mut(&client_id).expect("test client");
    out.extend(client.outbound.drain(..));
    out
}

fn poly_fill_rectangle_body(drawable: u32, gc: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&drawable.to_le_bytes());
    body.extend_from_slice(&gc.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body
}

fn free_pixmap_body(pixmap: u32) -> Vec<u8> {
    pixmap.to_le_bytes().to_vec()
}

/// Wire body for a ConfigureWindow request: window, value_mask, the
/// 2-byte request pad, then one CARD32 per set value-mask bit in bit
/// order (x,y,w,h,border,sibling,stack_mode).
fn cw_restack_body(window: u32, value_mask: u16, values: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&value_mask.to_le_bytes());
    body.extend_from_slice(&[0u8, 0u8]); // request pad
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

/// Two children (0x200 created first, then 0x300 → 0x300 ends on top)
/// under root, plus client 1's drained peer socket.
fn two_children_under_root() -> (ServerState, UnixStream) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, 1);
    for id in [0x200u32, 0x300u32] {
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ROOT_WINDOW,
                width: 50,
                height: 50,
                ..Default::default()
            },
        );
    }
    (state, peer)
}

/// Extract the `mode` byte (wire offset 11) of every Present
/// `CompleteNotify` in a raw byte stream, in wire order — an
/// emit_idle-independent delivery-order oracle for tests mixing Copy
/// and Skip (Skip's gated `signal_present_wake` means
/// `RecordingBackend::signalled_present_wakes` alone can't order
/// them). Callers must select ONLY `EVENT_MASK_COMPLETE_NOTIFY` (no
/// `IdleNotify`/`ConfigureNotify`), so every event in the stream is a
/// fixed 40-byte `CompleteNotify` and the chunking can't desync.
fn complete_notify_modes(bytes: &[u8]) -> Vec<u8> {
    bytes
        .chunks(40)
        .map(|chunk| {
            assert_eq!(chunk.len(), 40, "truncated CompleteNotify");
            assert_eq!(chunk[0], 35, "GenericEvent");
            assert_eq!(chunk[1], 145, "PRESENT major opcode");
            chunk[11]
        })
        .collect()
}

/// A request body in the client's byte order.
struct WireBody(ClientByteOrder, Vec<u8>);

impl WireBody {
    fn u32(mut self, v: u32) -> Self {
        match self.0 {
            ClientByteOrder::LittleEndian => self.1.extend_from_slice(&v.to_le_bytes()),
            ClientByteOrder::BigEndian => self.1.extend_from_slice(&v.to_be_bytes()),
        }
        self
    }
    fn u16(mut self, v: u16) -> Self {
        match self.0 {
            ClientByteOrder::LittleEndian => self.1.extend_from_slice(&v.to_le_bytes()),
            ClientByteOrder::BigEndian => self.1.extend_from_slice(&v.to_be_bytes()),
        }
        self
    }
    fn bytes(mut self, v: &[u8]) -> Self {
        self.1.extend_from_slice(v);
        self
    }
}

mod x11randr_minor {
    pub const SET_MONITOR: u8 = yserver_protocol::x11::randr::RR_SET_MONITOR;
    pub const DELETE_MONITOR: u8 = yserver_protocol::x11::randr::RR_DELETE_MONITOR;
}

fn rr_scale(word: i32) -> [i32; 9] {
    [word, 0, 0, 0, word, 0, 0, 0, 0x0001_0000]
}
const MUFFIN_2_0: i32 = 131_072;

fn host_frees(backend: &RecordingBackend) -> Vec<u32> {
    let mut out: Vec<u32> = backend
        .calls()
        .iter()
        .filter_map(|call| match call {
            RecordedCall::FreePixmap(xid) => Some(*xid),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out
}

/// Drive one XI request through the core dispatcher and return its wire
/// reply. The actual XInput extension major opcode is 137; `minor` is the
/// XI 1.x / XI2 request number in the data byte.
fn dispatch_xi_request_wire(
    state: &mut ServerState,
    peer: &mut UnixStream,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    dispatch_wire_request(state, peer, sequence, XI2_MAJOR_OPCODE, minor, body)
}

/// Drive any core or extension request through `process_request` and
/// return the bytes written for it.
fn dispatch_wire_request(
    state: &mut ServerState,
    peer: &mut UnixStream,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    let mut backend = RecordingBackend::new();
    process_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("request through core dispatcher");
    read_all_available(peer)
}

/// Read-only XI replies must leave the registry, holds, floating state,
/// property maps, and every client selection unchanged.
fn xi_query_side_effect_snapshot(state: &ServerState, client_id: u32) -> String {
    let client = &state.clients[&client_id];
    format!(
        "registry={:?}; keys={:?}; buttons={}; device_buttons={:?}; \
             detached={:?}; floating={:?}; properties={:?}; selections={:?}",
        state.xi_devices,
        state.keys_down,
        state.buttons_down,
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, device.buttons_down))
            .collect::<Vec<_>>(),
        state.xi2_detached_masters,
        state.floating_pointer_positions,
        state
            .xi_devices
            .devices()
            .iter()
            .map(|device| (device.id, &device.properties))
            .collect::<Vec<_>>(),
        (
            &client.event_masks,
            &client.xi2_masks,
            &client.xi1_event_classes,
            &client.xi1_window_event_classes,
        ),
    )
}

// -----------------------------------------------------------------
// Tier 2 Task 3: XI2 device-property requests (dispatch-level, wire bytes)
// -----------------------------------------------------------------

/// Header for an XI2 (major 131) request with the given minor opcode.
fn xi2_header(minor: u8) -> RequestHeader {
    // Declare the spec-correct request length so the dispatcher's
    // REQUEST_SIZE_MATCH gate (validate_xi_request_length) doesn't
    // BadLength the request before it reaches the handler. These
    // fixtures pre-date that XTS-driven length gate and used a
    // placeholder length_units=1; for Fixed-size minors the table
    // value IS the exact wire length, for variable minors it is the
    // spec minimum (payload-carrying variable requests build their
    // own header — see xi2_header_for_body).
    use yserver_protocol::x11::request_lengths::{LenSpec, xi_request_length};
    let length_units = match xi_request_length(minor) {
        Some(LenSpec::Fixed(n) | LenSpec::AtLeast(n)) => n,
        None => 1,
    };
    RequestHeader {
        opcode: 131,
        data: minor,
        length_units,
    }
}

/// Header for a variable-length XI minor whose body carries a payload
/// (e.g. XIChangeProperty value bytes): the exact-length gate requires
/// `length_units` to match the actual body, so derive it from `body`.
fn xi2_header_for_body(minor: u8, body: &[u8]) -> RequestHeader {
    RequestHeader {
        opcode: 131,
        data: minor,
        length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap_or(1),
    }
}

/// Seed the XTEST pointer (id 4) with one INTEGER/8 property
/// keyed by a synthetic atom whose numeric id is supplied by the
/// caller. The atom is registered into `state.atoms` so it passes
/// the T3 BadAtom guard in the property dispatch arms — historical
/// test fixtures pass 100/200, which sit outside the well-known
/// range and would otherwise look "uninterned" to the dispatcher.
fn seed_one_prop(state: &mut ServerState, atom: u32, format: u8, data: Vec<u8>) {
    state
        .atoms
        .register_for_test(AtomId(atom), &format!("test-prop-{atom}"));
    let dev = state
        .xi_devices
        .device_mut(crate::xinput::DEVICEID_XTEST_POINTER)
        .expect("XTEST pointer");
    dev.properties.insert(
        AtomId(atom),
        crate::xinput::XiProperty {
            type_atom: crate::xinput::XA_INTEGER,
            format,
            data,
            read_only: false,
            deletable: true,
        },
    );
}

// -----------------------------------------------------------------
// T3 dispatch: validate→apply→commit + BadAtom/BadAccess/BadValue.
// These exercise the new descriptor-aware path against a seeded
// touchpad; the recording backend's default `apply_device_config`
// returns `Ok(())`, so successful writes commit to the registry.
// -----------------------------------------------------------------

/// Build an `xXIChangePropertyReq` body (after the 4-byte generic
/// header): deviceid(2), mode(1), format(1), property(4), type(4),
/// num_items(4), value(num_items * format/8). The caller is
/// responsible for any 4-byte pad on the wire — the body slice we
/// hand to `handle_xi2_request` doesn't need it because each test
/// passes the raw data block directly.
fn xi2_change_property_body(
    deviceid: u16,
    mode: u8,
    format: u8,
    property: u32,
    type_atom: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&deviceid.to_le_bytes());
    body.push(mode);
    body.push(format);
    body.extend_from_slice(&property.to_le_bytes());
    body.extend_from_slice(&type_atom.to_le_bytes());
    let num_items = data.len() / usize::from(format / 8);
    body.extend_from_slice(&(num_items as u32).to_le_bytes());
    body.extend_from_slice(data);
    // 4-byte pad on the value tail.
    while body.len() % 4 != 0 {
        body.push(0);
    }
    body
}

const TEST_PHYSICAL_POINTER_ID: u16 = 6;

/// Register a source and return its registry-owned pointer facet for
/// property dispatch fixtures.
fn seed_physical_pointer_fixture(
    state: &mut ServerState,
    info: &crate::core_loop::DeviceInfo,
) -> u16 {
    state
        .xi_register_source(info)
        .into_iter()
        .find(|id| {
            state.xi_devices.device(*id).is_some_and(|device| {
                device.facet == Some(crate::xinput::XiFacetKind::PointerTouch)
            })
        })
        .expect("test source publishes a pointer facet")
}

/// Seed a touchpad on physical XI ID 6 with tap available+current+default.
/// Mirrors the MATE test fixture without using virtual XTEST device 4.
fn seed_pointer_for_t3(state: &mut ServerState) -> u16 {
    let info = crate::core_loop::DeviceInfo {
        source_id: crate::xinput::InputSourceId(u64::from(line!())),
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard: false,
            pointer: true,
            touch: false,
        },
        name: "SynPS/2 Synaptics TouchPad".into(),
        device_node: "/dev/input/event4".into(),
        sysname: "event4".into(),
        vendor_id: 0x046d,
        product_id: 0xc52f,
        is_touchpad: true,
        config: crate::core_loop::message::LibinputConfigSnapshot {
            tap: crate::core_loop::message::BoolSetting {
                available: true,
                current: true,
                default: false,
            },
            natural_scroll: crate::core_loop::message::BoolSetting {
                available: true,
                current: false,
                default: true,
            },
            dwt: crate::core_loop::message::BoolSetting {
                available: true,
                current: true,
                default: true,
            },
            scroll_method: crate::core_loop::message::OneHot3 {
                available_mask: 0b111,
                current: Some(0),
                default: Some(0),
            },
            accel: crate::core_loop::message::FloatSetting {
                available: true,
                current: 0.0,
                default: 0.0,
            },
            scroll_button: crate::core_loop::message::U32Setting {
                available: true,
                current: 0,
                default: 0,
            },
            accel_profile: crate::core_loop::message::OneHot2 {
                available: true,
                current: Some(0),
                default: Some(0),
            },
            accel_profile_available_mask: 0b011,
            ..Default::default()
        },
    };
    seed_physical_pointer_fixture(state, &info)
}

fn inventory_for_t3_source(
    state: &ServerState,
) -> (
    crate::core_loop::input_inventory::InputInventory,
    crate::xinput::InputSourceId,
) {
    let source = state
        .xi_devices
        .device(TEST_PHYSICAL_POINTER_ID)
        .and_then(|device| device.source_id)
        .expect("physical pointer source");
    let mut inventory = crate::core_loop::input_inventory::InputInventory::new();
    inventory.add(
        state
            .xi_devices
            .source(source)
            .expect("source facts")
            .clone(),
    );
    (inventory, source)
}

fn route_t3_config_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    inventory: &mut crate::core_loop::input_inventory::InputInventory,
    pending: &mut crate::core_loop::run::PendingBackendRequests,
    lane: &mut crate::core_loop::run::XiConfigLane,
    request: crate::core_loop::message::XiConfigRequest,
) {
    crate::core_loop::run::route_pending_xi_config(
        state,
        backend,
        inventory,
        pending,
        lane,
        &mut crate::core_loop::reset::ResetTrigger::new(
            crate::core_loop::reset::ResetPolicy::NoReset,
        ),
        request,
        32,
        crate::core_loop::generation::Generation::default(),
    );
}

fn xi_property_snapshot(
    state: &ServerState,
) -> Vec<(
    u16,
    std::collections::BTreeMap<AtomId, crate::xinput::XiProperty>,
)> {
    state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect()
}

/// Parse a recognized XI1/XI2 write, prove parsing has no side effects,
/// and then drive the production core lane. This is the shared route for
/// the migrated property-dispatch tests.
fn drive_t3_config_wire_request(
    state: &mut ServerState,
    peer: &mut impl TestPeer,
    backend: &mut RecordingBackend,
    inventory: &mut crate::core_loop::input_inventory::InputInventory,
    pending: &mut crate::core_loop::run::PendingBackendRequests,
    lane: &mut crate::core_loop::run::XiConfigLane,
    client: u32,
    sequence: u16,
    header: RequestHeader,
    body: &[u8],
) -> crate::core_loop::message::XiConfigRequest {
    let properties_before = xi_property_snapshot(state);
    let calls_before = backend.started_device_configs.len();
    let outcome = handle_xi2_request(
        state,
        backend,
        None,
        ClientId(client),
        SequenceNumber(sequence),
        header,
        body,
    )
    .expect("parse XI property request");
    let RequestOutcome::PendingXiConfig(request) = outcome else {
        panic!("recognized physical write must return PendingXiConfig, got {outcome:?}");
    };
    assert_eq!(
        xi_property_snapshot(state),
        properties_before,
        "parsing does not mutate XI properties"
    );
    assert_eq!(
        backend.started_device_configs.len(),
        calls_before,
        "parsing does not call the backend"
    );
    assert!(
        read_all_available(peer).is_empty(),
        "parsing produces no reply, error, or property notification"
    );
    let captured = request.clone();
    route_t3_config_request(state, backend, inventory, pending, lane, request);
    captured
}

fn assert_xi_config_error(wire: &[u8], code: u8, sequence: u16, minor_opcode: u16) {
    assert_eq!(wire.len(), 32, "one X error packet");
    assert_eq!(wire[0], 0, "X_Error");
    assert_eq!(wire[1], code, "error code");
    assert_eq!(
        u16::from_le_bytes([wire[2], wire[3]]),
        sequence,
        "original request sequence"
    );
    assert_eq!(
        u16::from_le_bytes([wire[8], wire[9]]),
        minor_opcode,
        "original XI minor opcode"
    );
    assert_eq!(wire[10], 137, "XInput extension major opcode");
}

fn assert_confirmed_tap_write(
    state: &ServerState,
    inventory: &crate::core_loop::input_inventory::InputInventory,
    source: crate::xinput::InputSourceId,
    property: AtomId,
) {
    assert_eq!(
        state
            .xi_devices
            .device(TEST_PHYSICAL_POINTER_ID)
            .unwrap()
            .properties[&property]
            .data,
        [0],
        "confirmed tap write commits the requested XI bytes"
    );
    assert!(!inventory.get(source).unwrap().config.tap.current);
}

// -----------------------------------------------------------------
// XI2 XI_PropertyEvent fan-out on change / delete / get(delete=1).
// The dispatch arm must wire `emit_property_change` into all six
// property-write sites so every client that selected
// `XI_PropertyEventMask` on the affected device receives a
// 32-byte XI2 GenericEvent (evtype 12) with the right `what` byte.
// -----------------------------------------------------------------

/// Scan a wire buffer for the first XI2 GenericEvent (`type == 35`)
/// of evtype 12 (XI_PropertyEvent) and return the 32-byte slice
/// pointing at the event header. Used by the T5 tests below; we
/// can't anchor at offset 0 because some arms emit a reply (e.g.
/// `XIGetProperty(delete=1)`) before the event.
///
/// Window: `0..=wire.len()-32` (inclusive) so a 32-byte buffer
/// matches at offset 0.
fn find_xi2_property_event(wire: &[u8]) -> Option<&[u8]> {
    let end = wire.len().checked_sub(32)?;
    (0..=end).find_map(|i| {
        (wire[i] == 35 && u16::from_le_bytes([wire[i + 8], wire[i + 9]]) == 12)
            .then(|| &wire[i..i + 32])
    })
}

/// Subscribe `client_id` to `XI_PropertyEventMask` for one device at
/// the root window — the selection MATE / GDK make after learning its id.
fn select_xi2_property_event_on_root(state: &mut ServerState, client_id: u32, deviceid: u16) {
    state
        .clients
        .get_mut(&client_id)
        .expect("client installed")
        .xi2_masks
        .insert((ROOT_WINDOW, deviceid), u64::from(XI2_PROPERTY_EVENT_MASK));
}

fn xi_hotplug_dispatch(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    sequence: u16,
    opcode: u8,
    minor: u8,
    body: &[u8],
) {
    let header = RequestHeader {
        opcode,
        data: minor,
        length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap_or(1),
    };
    let outcome = process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(sequence),
        header,
        body,
        None,
    )
    .expect("hotplug test request dispatch");
    assert!(
        matches!(outcome, RequestOutcome::Handled),
        "hotplug request should complete synchronously: {outcome:?}"
    );
}

fn xi_hotplug_create_window_body(window: u32, parent: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&parent.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes()); // x
    body.extend_from_slice(&0i16.to_le_bytes()); // y
    body.extend_from_slice(&32u16.to_le_bytes()); // width
    body.extend_from_slice(&32u16.to_le_bytes()); // height
    body.extend_from_slice(&0u16.to_le_bytes()); // border_width
    body.extend_from_slice(&0u16.to_le_bytes()); // CopyFromParent class
    body.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
    body.extend_from_slice(&0u32.to_le_bytes()); // no attributes
    body
}

/// Build an `xSelectExtensionEventReq` body (the bytes after the
/// 4-byte X request header) for a single XEventClass list.
fn xi1_select_extension_event_body(window: u32, classes: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + classes.len() * 4);
    body.extend_from_slice(&window.to_le_bytes()); // window
    #[allow(clippy::cast_possible_truncation)]
    let count = classes.len() as u16;
    body.extend_from_slice(&count.to_le_bytes()); // count
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    for c in classes {
        body.extend_from_slice(&c.to_le_bytes());
    }
    body
}

fn read_error(buf: &[u8]) -> u8 {
    // X error: type=0, error-code at byte 1.
    assert_eq!(buf[0], 0, "expected an X error, got type {}", buf[0]);
    buf[1]
}

fn create_root_child(state: &mut ServerState, window: u32) {
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(window),
            parent: ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
}

/// A minimal `PendingPresentEntry` for `present_pending_exec`, only
/// carrying what the Task 6 hold-back tests below need: which window
/// it targets. Everything else is inert filler.
fn stub_pending_present_entry(window: u32, present_id: u64) -> crate::server::PendingPresentEntry {
    use crate::server::{PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest};
    use yserver_protocol::x11::present::PixmapRequest;

    PendingPresentEntry {
        pending: PendingPresentPixmap {
            origin: None,
            client_id: ClientId(1),
            request: PendingPresentRequest::Pixmap(PixmapRequest {
                window,
                pixmap: 0x1,
                serial: 1,
                valid: 0,
                update: 0,
                x_off: 0,
                y_off: 0,
                target_crtc: 0,
                wait_fence: 0,
                idle_fence: 0,
                options: 0,
                target_msc: 0,
                divisor: 0,
                remainder: 0,
                notifies: Vec::new(),
            }),
            wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
            masked_options: 0,
            src_host_xid: 0x2,
            // Deliberately DISTINCT from `window` (the client-visible
            // XID the blocked-check keys on via `request.window()`):
            // if a future edit mistakenly keyed the hold-back off a
            // host-xid field instead, these tests must fail rather
            // than pass by accident on identical values.
            paint_dst_host_xid: window | 0x0040_0000,
            completion_dst_host_xid: window | 0x0040_0000,
            src_width: 1,
            src_height: 1,
            update_rects: None,
            present_id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: None,
        },
        source_ready: false,
        wait_id: None,
        pin: None,
    }
}

/// Builds a minimal `PendingPresentEntry` for the msc-due tests below —
/// deliberately distinct host xids for source/dest (window_host_xid |
/// 0 stays the "destination", pixmap_host_xid the "source") so a
/// `CopyArea` assertion can't pass by coincidence.
fn present_pending_entry_with(
    present_id: u64,
    window_host_xid: u32,
    pixmap_host_xid: u32,
    effective_target_msc: Option<u64>,
    source_ready: bool,
) -> crate::server::PendingPresentEntry {
    use crate::server::{PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest};
    use yserver_protocol::x11::present::PixmapRequest;

    PendingPresentEntry {
        pending: PendingPresentPixmap {
            origin: None,
            client_id: ClientId(1),
            request: PendingPresentRequest::Pixmap(PixmapRequest {
                window: window_host_xid,
                pixmap: pixmap_host_xid,
                serial: 1,
                valid: 0,
                update: 0,
                x_off: 0,
                y_off: 0,
                target_crtc: 0,
                wait_fence: 0,
                idle_fence: 0,
                options: 0,
                target_msc: 0,
                divisor: 0,
                remainder: 0,
                notifies: Vec::new(),
            }),
            wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
            masked_options: 0,
            src_host_xid: pixmap_host_xid,
            paint_dst_host_xid: window_host_xid,
            completion_dst_host_xid: window_host_xid,
            src_width: 4,
            src_height: 4,
            update_rects: None,
            present_id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc,
        },
        source_ready,
        wait_id: None,
        pin: Some(present_id), // distinct-ish pin id for release assertions
    }
}

// =====================================================================
// Task 8: supersession (coverage-gated scrap + parked Skip) and the
// copy-failure reroute. Spec §Supersession + §"Ordered completion
// delivery"; plan `docs/superpowers/plans/
// 2026-08-01-present-deferred-execution-supersession.md` Task 8.
// =====================================================================

use yserver_protocol::x11::xfixes;

/// Full-control `PendingPresentPixmap`/`PendingPresentEntry` builder
/// for the Task 8 tests below — unlike `stub_pending_present_entry`
/// (window/present_id only) and `present_pending_entry_with`
/// (window/pixmap host xids/eff/source_ready only), these tests need
/// to vary geometry, update region, and wake identity (`Pixmap`
/// idle_fence vs `PixmapSynced` release syncobj).
#[derive(Clone)]
struct SupersessionFixture {
    present_id: u64,
    window: u32,
    effective_target_msc: Option<u64>,
    x_off: i16,
    y_off: i16,
    src_width: u16,
    src_height: u16,
    update_rects: Option<Vec<xfixes::RegionRect>>,
    /// The wire `update` field (an XFixes region XID or 0) —
    /// independent of `update_rects` (the server-resolved rects, or
    /// `None` when the region was unresolvable). Task 13: needed to
    /// exercise `execute_present_pixmap_copy`'s damage arm with
    /// `update != 0` + `update_rects == None` (unresolvable region).
    update: u32,
    idle_fence: u32,
    synced_release: Option<(u32, u64)>,
}

impl SupersessionFixture {
    fn new(present_id: u64, window: u32) -> Self {
        Self {
            present_id,
            window,
            effective_target_msc: Some(100),
            x_off: 0,
            y_off: 0,
            src_width: 100,
            src_height: 100,
            update_rects: None,
            update: 0,
            idle_fence: 0,
            synced_release: None,
        }
    }

    fn eff(mut self, eff: Option<u64>) -> Self {
        self.effective_target_msc = eff;
        self
    }

    fn geometry(mut self, x_off: i16, y_off: i16, src_width: u16, src_height: u16) -> Self {
        self.x_off = x_off;
        self.y_off = y_off;
        self.src_width = src_width;
        self.src_height = src_height;
        self
    }

    fn update_rects(mut self, rects: Option<Vec<xfixes::RegionRect>>) -> Self {
        self.update_rects = rects;
        self
    }

    fn update(mut self, update: u32) -> Self {
        self.update = update;
        self
    }

    fn idle_fence(mut self, fence: u32) -> Self {
        self.idle_fence = fence;
        self
    }

    fn synced_release(mut self, release_syncobj: u32, release_value: u64) -> Self {
        self.synced_release = Some((release_syncobj, release_value));
        self
    }

    fn pending(&self) -> crate::server::PendingPresentPixmap {
        use crate::server::{PendingPresentPixmap, PendingPresentRequest};
        use yserver_protocol::x11::present::{PixmapRequest, PixmapSyncedRequest};

        let request = if let Some((release_syncobj, release_value)) = self.synced_release {
            PendingPresentRequest::PixmapSynced(PixmapSyncedRequest {
                window: self.window,
                pixmap: 0x1,
                serial: 1,
                valid: 0,
                update: self.update,
                x_off: self.x_off,
                y_off: self.y_off,
                target_crtc: 0,
                acquire_syncobj: 0,
                release_syncobj,
                acquire_value: 0,
                release_value,
                options: 0,
                target_msc: 0,
                divisor: 0,
                remainder: 0,
                notifies: Vec::new(),
            })
        } else {
            PendingPresentRequest::Pixmap(PixmapRequest {
                window: self.window,
                pixmap: 0x1,
                serial: 1,
                valid: 0,
                update: self.update,
                x_off: self.x_off,
                y_off: self.y_off,
                target_crtc: 0,
                wait_fence: 0,
                idle_fence: self.idle_fence,
                options: 0,
                target_msc: 0,
                divisor: 0,
                remainder: 0,
                notifies: Vec::new(),
            })
        };

        PendingPresentPixmap {
            origin: None,
            client_id: ClientId(1),
            request,
            wake: crate::backend::PresentWake::Pixmap {
                idle_fence_xid: self.idle_fence,
            },
            masked_options: 0,
            src_host_xid: 0x2,
            paint_dst_host_xid: self.window | 0x0040_0000,
            completion_dst_host_xid: self.window | 0x0040_0000,
            src_width: self.src_width,
            src_height: self.src_height,
            update_rects: self.update_rects.clone(),
            present_id: self.present_id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: self.effective_target_msc,
        }
    }

    fn entry(&self) -> crate::server::PendingPresentEntry {
        crate::server::PendingPresentEntry {
            pending: self.pending(),
            source_ready: true,
            wait_id: None,
            pin: None,
        }
    }
}

// ---------------- Stage 4d gate: COW host_xid wiring ----------------
//
// Marco-with-compositing uses **pure PRESENT-pixmap onto COW** with zero
// `RedirectSubwindows` / `NameWindowPixmap` calls — it repeatedly emits
// `PRESENT::Pixmap(window=0x103 /* COW */, pixmap=client_offscreen)`.
//
// `process_present_pixmap` resolves both endpoints via
// `state.resources.host_drawable_target(...)`, which returns `None` when
// `window.host_xid` is `None`. Stage 4d allocated COW storage on the
// backend (`KmsBackend.cow_id`) and registered with the scene, but
// left `host_xid` `None` on the yserver-core resource record (seeded
// that way at `ResourceTable::new` since the COW xid pre-exists the
// backend-side allocation). Every `PresentPixmap → COW` therefore
// silently fell through the `if let (Some, Some)` guard at
// `process_request.rs:~4124` and dropped paint.
//
// Fix: on the GET_OVERLAY_WINDOW arm, after the backend hook lands
// successfully, wire `host_xid = Some(0x103)` and copy root dimensions
// onto the COW resource record. On the final RELEASE_OVERLAY_WINDOW,
// destroy the record so the next GET re-wires fresh storage.
//
// These tests use `RecordingBackend`. Which release is final is
// decided by core's claim list (`ServerState::cow_claims`), not by
// the backend — the backend counts nothing.

fn dispatch_composite_minor(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    client_id: ClientId,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> RequestOutcome {
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    process_request(
        state,
        backend,
        client_id,
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 144, // COMPOSITE
            data: minor,
            length_units,
        },
        body,
        None,
    )
    .unwrap()
}

fn xi_dynamic_grab_source(
    state: &mut ServerState,
    source: u64,
    keyboard: bool,
    pointer: bool,
    name: &str,
) -> (crate::xinput::InputSourceId, u16) {
    let source_id = crate::xinput::InputSourceId(source);
    let info = crate::core_loop::DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: crate::xinput::InputCapabilities {
            keyboard,
            pointer,
            touch: false,
        },
        name: name.to_owned(),
        device_node: format!("/dev/input/{name}"),
        sysname: name.to_owned(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: crate::core_loop::message::LibinputConfigSnapshot::default(),
    };
    let facet = if keyboard {
        crate::xinput::XiFacetKind::Keyboard
    } else {
        crate::xinput::XiFacetKind::PointerTouch
    };
    let ids = state.xi_register_source(&info);
    let device_id = ids
        .into_iter()
        .find(|id| {
            state
                .xi_devices
                .device(*id)
                .is_some_and(|device| device.facet == Some(facet))
        })
        .expect("requested physical facet was registered");
    (source_id, device_id)
}

/// Split queued 32-byte packets (errors, events, fixed replies).
fn wire_packets(peer: &mut UnixStream) -> Vec<[u8; 32]> {
    read_all_available(peer)
        .chunks_exact(32)
        .map(|c| <[u8; 32]>::try_from(c).unwrap())
        .collect()
}

fn le_u32(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().unwrap())
}

/// Error packet → (code, bad value, minor, major).
fn error_fields(p: &[u8; 32]) -> (u8, u32, u16, u8) {
    assert_eq!(p[0], 0, "expected an error packet, got type {}", p[0]);
    (p[1], le_u32(p, 4), u16::from_le_bytes([p[8], p[9]]), p[10])
}

/// Seed a `Window` resource under `parent` with the given size
/// and a deterministic `host_xid` derived from the nested xid.
/// Mirrors what the production `CreateWindow` path leaves in
/// `state.resources` once the host create has roundtripped.
fn seed_window(
    state: &mut ServerState,
    xid: ResourceId,
    parent: ResourceId,
    width: u16,
    height: u16,
) {
    use yserver_protocol::x11::CreateWindowRequest;
    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            depth: 24,
            window: xid,
            parent,
            x: 0,
            y: 0,
            width,
            height,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    if let Some(w) = state.resources.window_mut(xid) {
        // Synthesise a host_xid from the nested xid (high bit
        // set so it can't collide with a low-numbered nested
        // xid in a different role). Production assigns these
        // out of the backend; tests just need stable values.
        w.host_xid = crate::backend::WindowHandle::from_raw(0x8000_0000 | xid.0);
    }
    // As CreateWindow does.
    state
        .composite_redirects
        .redirect_new_subwindow(parent, xid);
}

// ── Border attribute validation (#133, plan step 1.3 / 1.4) ────────

const CWA_BORDER_PIXMAP: u32 = 0x0004;
const CWA_BORDER_PIXEL: u32 = 0x0008;

fn border_cwa_body(window: u32, value_mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + values.len() * 4);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&value_mask.to_le_bytes());
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

fn run_border_request(state: &mut ServerState, opcode: u8, depth: u8, body: &[u8]) {
    run_border_request_recording(state, opcode, depth, body);
}

/// Same as `run_border_request` but hands back what the backend
/// saw, for the #133 step 2 (P3) forward-path assertions.
fn run_border_request_recording(
    state: &mut ServerState,
    opcode: u8,
    depth: u8,
    body: &[u8],
) -> Vec<RecordedCall> {
    let mut backend = RecordingBackend::new();
    process_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data: depth,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
    backend.calls()
}

fn assert_error_code(bytes: &[u8], expected: u8, what: &str) {
    assert!(
        bytes.len() >= 32,
        "{what}: expected a 32-byte error reply, got {} bytes: {:02x?}",
        bytes.len(),
        bytes
    );
    assert_eq!(bytes[0], 0, "{what}: expected an Error reply");
    assert_eq!(bytes[1], expected, "{what}: wrong error code");
}

fn assert_no_error(bytes: &[u8], what: &str) {
    assert!(
        bytes.is_empty() || bytes[0] != 0,
        "{what}: expected no error, got {:02x?}",
        bytes
    );
}
