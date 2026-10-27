//! `RecordingHost`: a `ServerState` + `RecordingBackend` driven by wire
//! requests, standing in for a rootless host.

mod acks;
mod info;
mod seat;
mod topology;
mod wm;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    sync::{Arc, Mutex, atomic::AtomicU16},
};

use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

use crate::{
    backend::recording::{RecordedCall, RecordingBackend},
    core_loop::process_request::process_request,
    resources::ROOT_WINDOW,
    rootless::{self, CoordinatePolicy, RootlessEvent, ToplevelId},
    server::{ClientState, ServerState},
    transport::{CapturedPeer, Transport},
};

pub(super) struct RecordingHost {
    pub state: ServerState,
    pub backend: RecordingBackend,
    peers: HashMap<u32, CapturedPeer>,
    sequence: u16,
}

impl RecordingHost {
    /// Rootless off; client 1 connected (little-endian).
    pub fn rooted() -> Self {
        let mut host = Self {
            state: ServerState::new(),
            backend: RecordingBackend::new().with_redirect_activation(),
            peers: HashMap::new(),
            sequence: 0,
        };
        host.connect(1, ClientByteOrder::LittleEndian);
        host
    }

    /// Rootless on (Opaque policy); client 1 connected.
    pub fn new() -> Self {
        let mut host = Self::rooted();
        host.enable();
        host
    }

    /// Rootless on with the Global policy (macOS, Windows).
    pub fn global() -> Self {
        let mut host = Self::rooted();
        rootless::enable(
            &mut host.state,
            &mut host.backend,
            CoordinatePolicy::Global { origin: (0, 0) },
        )
        .expect("enable rootless");
        host
    }

    pub fn enable(&mut self) {
        rootless::enable(&mut self.state, &mut self.backend, CoordinatePolicy::Opaque)
            .expect("enable rootless");
    }

    /// A host request, as `Message::Rootless` dispatches it.
    pub fn host(&mut self, request: rootless::HostRequest) {
        rootless::host_request(&mut self.state, &mut self.backend, request);
    }

    pub fn connect(&mut self, id: u32, byte_order: ClientByteOrder) {
        let (transport, peer) = Transport::capture_pair();
        self.state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(transport)),
                byte_order,
                last_sequence: Arc::new(AtomicU16::new(0)),
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
                gone: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
                internal: false,
            },
        );
        self.peers.insert(id, peer);
    }

    pub fn disconnect(&mut self, id: u32) {
        crate::core_loop::process_disconnect::process_disconnect(
            &mut self.state,
            &mut self.backend,
            ClientId(id),
        );
        self.peers.remove(&id);
    }

    /// Run one request from `client`, its body in the client's byte order,
    /// through the reader's swap; return what the client received.
    pub fn request(&mut self, client: u32, opcode: u8, data: u8, body: &[u8]) -> Vec<u8> {
        let mut body = body.to_vec();
        let order = self.state.clients[&client].byte_order;
        yserver_protocol::x11::request_swap::swap_request_body(opcode, data, order, &mut body);
        let body = body.as_slice();
        self.sequence = self.sequence.wrapping_add(1);
        process_request(
            &mut self.state,
            &mut self.backend,
            ClientId(client),
            SequenceNumber(self.sequence),
            RequestHeader {
                opcode,
                data,
                length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
            },
            body,
            None,
        )
        .expect("process_request");
        self.read(client)
    }

    pub fn read(&mut self, client: u32) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(peer) = self.peers.get_mut(&client) {
            peer.set_nonblocking(true).unwrap();
            let mut buf = [0u8; 4096];
            loop {
                match peer.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                }
            }
        }
        if let Some(c) = self.state.clients.get_mut(&client) {
            out.extend(c.outbound.drain(..));
        }
        out
    }

    /// CreateWindow, InputOutput, CopyFromParent visual, depth 24.
    #[allow(clippy::too_many_arguments)]
    pub fn create_window(
        &mut self,
        client: u32,
        wid: u32,
        parent: u32,
        (x, y): (i16, i16),
        (width, height): (u16, u16),
        border: u16,
        override_redirect: bool,
    ) {
        let mut body = Vec::new();
        body.extend_from_slice(&wid.to_le_bytes());
        body.extend_from_slice(&parent.to_le_bytes());
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.extend_from_slice(&width.to_le_bytes());
        body.extend_from_slice(&height.to_le_bytes());
        body.extend_from_slice(&border.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
        body.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent
        if override_redirect {
            body.extend_from_slice(&0x200u32.to_le_bytes()); // CWOverrideRedirect
            body.extend_from_slice(&1u32.to_le_bytes());
        } else {
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        let out = self.request(client, 1, 24, &body);
        assert_no_error(&out, "CreateWindow");
    }

    pub fn map(&mut self, client: u32, window: u32) {
        let out = self.request(client, 8, 0, &window.to_le_bytes());
        assert_no_error(&out, "MapWindow");
    }

    pub fn unmap(&mut self, client: u32, window: u32) {
        let out = self.request(client, 10, 0, &window.to_le_bytes());
        assert_no_error(&out, "UnmapWindow");
    }

    pub fn destroy(&mut self, client: u32, window: u32) {
        let out = self.request(client, 4, 0, &window.to_le_bytes());
        assert_no_error(&out, "DestroyWindow");
    }

    pub fn reparent(&mut self, client: u32, window: u32, parent: u32, (x, y): (i16, i16)) {
        let mut body = window.to_le_bytes().to_vec();
        body.extend_from_slice(&parent.to_le_bytes());
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        let out = self.request(client, 7, 0, &body);
        assert_no_error(&out, "ReparentWindow");
    }

    /// ConfigureWindow with `values` in value-mask bit order.
    pub fn configure(&mut self, client: u32, window: u32, mask: u16, values: &[u32]) {
        let mut body = window.to_le_bytes().to_vec();
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        for v in values {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let out = self.request(client, 12, 0, &body);
        assert_no_error(&out, "ConfigureWindow");
    }

    /// SHAPE Rectangles, op Set, kind Bounding, Unsorted.
    pub fn shape_bounding(&mut self, client: u32, window: u32, rects: &[(i16, i16, u16, u16)]) {
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&[0, 0, 0, 0]);
        for (x, y, w, h) in rects {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
            body.extend_from_slice(&w.to_le_bytes());
            body.extend_from_slice(&h.to_le_bytes());
        }
        let out = self.request(client, 141, yserver_protocol::x11::shape::RECTANGLES, &body);
        assert_no_error(&out, "ShapeRectangles");
    }

    pub fn atom(&mut self, name: &str) -> u32 {
        self.state.atoms.intern(name, false).0
    }

    /// ChangeProperty (mode Replace) with format-`format` items, encoded in
    /// the client's byte order.
    pub fn change_property(
        &mut self,
        client: u32,
        window: u32,
        property: &str,
        r#type: &str,
        format: u8,
        items: &[u32],
    ) {
        let be = self.state.clients[&client].byte_order == ClientByteOrder::BigEndian;
        let word = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let (property, r#type) = (self.atom(property), self.atom(r#type));
        let mut body = Vec::new();
        for v in [window, property, r#type] {
            body.extend_from_slice(&word(v));
        }
        body.extend_from_slice(&[format, 0, 0, 0]);
        body.extend_from_slice(&word(u32::try_from(items.len()).unwrap()));
        for v in items {
            match format {
                8 => body.push(u8::try_from(*v).unwrap()),
                16 => {
                    let v = u16::try_from(*v).unwrap();
                    body.extend_from_slice(&if be { v.to_be_bytes() } else { v.to_le_bytes() });
                }
                _ => body.extend_from_slice(&word(*v)),
            }
        }
        body.resize(body.len().next_multiple_of(4), 0);
        let out = self.request(client, 18, 0, &body);
        assert_no_error(&out, "ChangeProperty");
    }

    pub fn change_property_bytes(
        &mut self,
        client: u32,
        window: u32,
        property: &str,
        r#type: &str,
        bytes: &[u8],
    ) {
        let items: Vec<u32> = bytes.iter().map(|b| u32::from(*b)).collect();
        self.change_property(client, window, property, r#type, 8, &items);
    }

    pub fn delete_property(&mut self, client: u32, window: u32, property: &str) {
        let property = self.atom(property);
        let mut body = window.to_le_bytes().to_vec();
        body.extend_from_slice(&property.to_le_bytes());
        let out = self.request(client, 19, 0, &body);
        assert_no_error(&out, "DeleteProperty");
    }

    /// One loop-tail reconcile; the events the backend received.
    pub fn reconcile(&mut self) -> Vec<RootlessEvent> {
        let before = self.backend.calls().len();
        rootless::reconcile_and_dispatch(&mut self.state, &mut self.backend);
        self.backend.calls()[before..]
            .iter()
            .find_map(|c| match c {
                RecordedCall::RootlessEvents(events) => Some(events.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Map and reconcile; the one `Mapped` id.
    pub fn map_and_reconcile(&mut self, client: u32, window: u32) -> ToplevelId {
        self.map(client, window);
        match self.reconcile().as_slice() {
            [RootlessEvent::Mapped { id, .. }] => *id,
            other => panic!("expected one Mapped: {other:?}"),
        }
    }
}

pub(super) fn assert_no_error(bytes: &[u8], what: &str) {
    for packet in bytes.chunks(32) {
        assert_ne!(packet[0], 0, "{what}: unexpected X error {}", packet[1]);
    }
}

/// The error code of the first packet, which must be an error.
pub(super) fn error_code(bytes: &[u8]) -> u8 {
    assert_eq!(bytes.first(), Some(&0), "expected an X error: {bytes:?}");
    bytes[1]
}
