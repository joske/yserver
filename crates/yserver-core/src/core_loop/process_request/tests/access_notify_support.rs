use super::{
    extensions::{RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{Decision, RequestOpcodes},
    backend::recording::RecordingBackend,
};
use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
};

pub(super) const NOTIFY_SENDER: ClientId = ClientId(1);
pub(super) const NOTIFY_RECEIVER_DENIED: ClientId = ClientId(2);
pub(super) const NOTIFY_RECEIVER_ALLOWED: ClientId = ClientId(3);
pub(super) const NOTIFY_WINDOW: ResourceId = ResourceId(0x0051_0001);
pub(super) const PROPERTY_CHANGE_MASK: u32 = 1 << 22;

pub(super) struct NotifyFixture {
    pub state: ServerState,
    pub backend: RecordingBackend,
    pub peers: HashMap<u32, UnixStream>,
}

impl NotifyFixture {
    pub(super) fn new() -> Self {
        let mut state = ServerState::new();
        let peers = [
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ]
        .into_iter()
        .map(|client| (client.0, install_client(&mut state, client.0)))
        .collect();
        Self {
            state,
            backend: RecordingBackend::new(),
            peers,
        }
    }

    pub(super) fn request(
        &mut self,
        client: ClientId,
        sequence: SequenceNumber,
        header: RequestHeader,
        body: &[u8],
    ) -> io::Result<RequestOutcome> {
        process_request(
            &mut self.state,
            &mut self.backend,
            client,
            sequence,
            header,
            body,
            None,
        )
    }

    pub(super) fn drain(&mut self, client: ClientId) -> Vec<u8> {
        let peer = self.peers.get_mut(&client.0).expect("fixture client peer");
        read_all_or_buffered(&mut self.state, client.0, peer)
    }

    pub(super) fn install_policy(
        &mut self,
        decide: impl Fn(usize, &RecordedExtensionCheck) -> Decision + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        self.state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
                Arc::clone(&calls),
                "",
                decide,
            )));
        calls
    }
}

pub(super) fn property_change_body(window: ResourceId, property: AtomId, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(20 + data.len().div_ceil(4) * 4);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&31_u32.to_le_bytes()); // STRING
    body.extend_from_slice(&[8, 0, 0, 0]);
    body.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    body.extend_from_slice(data);
    while !body.len().is_multiple_of(4) {
        body.push(0);
    }
    body
}

pub(super) fn select_property_change(fixture: &mut NotifyFixture) {
    fixture.state.resources.create_window(
        NOTIFY_RECEIVER_DENIED,
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: NOTIFY_WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("fixture client")
            .event_masks
            .insert(NOTIFY_WINDOW, PROPERTY_CHANGE_MASK);
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
        .byte_order = ClientByteOrder::LittleEndian;
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("allowed receiver")
        .last_sequence
        .store(0x5678, std::sync::atomic::Ordering::Relaxed);
    // Fix PropertyNotify's timestamp at zero for literal wire assertions.
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
}

pub(super) fn run_property_change(
    fixture: &mut NotifyFixture,
    sequence: u16,
    property: AtomId,
    data: &[u8],
) -> RequestOutcome {
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: u32::try_from(
                    (4 + property_change_body(NOTIFY_WINDOW, property, data).len()) / 4,
                )
                .unwrap(),
            },
            &property_change_body(NOTIFY_WINDOW, property, data),
        )
        .expect("ChangeProperty through production dispatcher")
}

pub(super) fn literal_property_notify(
    order: ClientByteOrder,
    sequence: u16,
    property: AtomId,
) -> Vec<u8> {
    let mut bytes = vec![28, 0]; // PropertyNotify, detail
    match order {
        ClientByteOrder::LittleEndian => {
            bytes.extend_from_slice(&sequence.to_le_bytes());
            bytes.extend_from_slice(&NOTIFY_WINDOW.0.to_le_bytes());
            bytes.extend_from_slice(&property.0.to_le_bytes());
        }
        ClientByteOrder::BigEndian => {
            bytes.extend_from_slice(&sequence.to_be_bytes());
            bytes.extend_from_slice(&NOTIFY_WINDOW.0.to_be_bytes());
            bytes.extend_from_slice(&property.0.to_be_bytes());
        }
    }
    bytes.extend_from_slice(&[0; 20]); // timestamp, state, and pad
    bytes
}

pub(super) fn request_opcodes(major: u8, minor: u8) -> Option<RequestOpcodes> {
    Some(RequestOpcodes { major, minor })
}
