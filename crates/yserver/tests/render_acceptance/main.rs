//! Render-backend acceptance integration tests (Stage 2f).
//!
//! Drives `KmsBackend` directly via its `Backend` trait and
//! asserts pixel-correctness against a CPU oracle. Functionally
//! equivalent to the Stage 2 plan's "synthetic harness binary"
//! that would drive PutImage / CopyArea / PolyFillRectangle /
//! GetImage through the X11 protocol — but skipping the X11
//! protocol layer because the correctness gate is at the
//! Backend-trait surface, not at the protocol-encoding layer.
//!
//! These tests are gated on a live Vulkan ICD (lavapipe is fine):
//!
//! ```text
//! VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
//!   cargo test -p yserver --test acceptance -- --ignored
//! ```
//!
//! User-run hardware smoke on bee + fuji
//! (`just yserver-xfce-hw`) is the
//! load-bearing Stage 2 close gate; this file covers the
//! correctness oracle that gates against pixel-level regressions.

#![cfg(target_os = "linux")]

mod backing_lifetime;
mod basics;
mod border;
mod border_protocol;
mod depth_alpha;
mod frame_builder;
mod glyphs;
mod include_inferiors;
mod masked_copy;
mod present;
mod readback_versions;
mod redirect;
mod resize;
mod submit_group;
mod traps;
mod window_storage;

use yserver::kms::render::KmsBackend;
use yserver_core::backend::{
    AnyHandle, Backend, ClipState, DrawState, FillState, GcFunction, SubwindowMode,
};
use yserver_protocol::x11::ClipRectangles;

/// Retire everything in flight, then collect delivered completions until
/// `done` holds or 5 s pass. `drain_all` waits only on engine-tracked
/// tickets; the signal-only submit's fence and the exported sync_file are
/// checked with a zero-timeout poll, so delivery can trail the drain.
fn drain_present_events_until(
    b: &mut KmsBackend,
    done: impl Fn(&[yserver_core::backend::CompletedPresentEvent]) -> bool,
) -> Vec<yserver_core::backend::CompletedPresentEvent> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    b.engine_drain_all_for_tests();
    let mut events = Vec::new();
    loop {
        events.extend(b.drain_completed_present_events_for_tests());
        if done(&events) || std::time::Instant::now() >= deadline {
            return events;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
const BRD_RED: u32 = 0xFFFF_0000;
const BRD_GREEN: u32 = 0xFF00_FF00;
const BRD_BLUE: u32 = 0xFF00_00FF;

/// X11 ARGB pixel → the BGRA bytes `GetImage` returns for depth 32.
fn brd_bgra(pixel: u32) -> [u8; 4] {
    [
        (pixel & 0xFF) as u8,
        ((pixel >> 8) & 0xFF) as u8,
        ((pixel >> 16) & 0xFF) as u8,
        ((pixel >> 24) & 0xFF) as u8,
    ]
}

// ───── #133 step 3 round 6 — protocol-level xts reproduction ─────
//
// The round-5 Backend-trait tests all pass while xts fails, so they do
// not model the scenario: xts drives the CORE (GC state, ClearArea,
// ConfigureWindow, GetImage replies), and only the core decides which
// backend calls happen at all. This harness runs the real
// `process_request` dispatcher against a real `KmsBackend`, so a
// purpose can be replayed request-for-request.

/// Minimal server + client fixture driving `process_request` into a
/// live `KmsBackend`.
struct ProtoFixture {
    state: yserver_core::server::ServerState,
    backend: KmsBackend,
    _peer: std::os::unix::net::UnixStream,
    seq: u16,
}

impl ProtoFixture {
    fn new() -> Option<Self> {
        use std::os::unix::net::UnixStream;
        use yserver_core::{
            resources::{ARGB_COLORMAP, ARGB_VISUAL, ROOT_VISUAL, ROOT_WINDOW},
            server::ServerState,
        };

        let backend = KmsBackend::for_tests_with_vk().ok()?;
        let mut state = ServerState::with_geometry(800, 600);
        // Mirror `install_backend_root_bindings` (yserver/src/lib.rs:29).
        if let Some(root) = state.resources.window_mut(ROOT_WINDOW) {
            root.host_xid = yserver_core::backend::WindowHandle::from_raw(backend.window_id());
        }
        state
            .resources
            .set_visual_host_xid(ROOT_VISUAL, backend.root_visual_xid());
        if let Some(cm) = backend.argb_colormap_xid() {
            state.resources.set_colormap_host_xid(ARGB_COLORMAP, cm);
        }
        if let Some(v) = backend.argb_visual_xid() {
            state.resources.set_visual_host_xid(ARGB_VISUAL, v);
        }

        let (a, b) = UnixStream::pair().ok()?;
        state.clients.insert(1, Self::client_state(a));
        Some(Self {
            state,
            backend,
            _peer: b,
            seq: 0,
        })
    }

    fn client_state(a: std::os::unix::net::UnixStream) -> yserver_core::server::ClientState {
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        use yserver_core::{resources::ROOT_WINDOW, server::ClientState};
        ClientState {
            writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(a))),
            byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
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
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        }
    }

    /// A second client, for tests where the compositor and the app are different clients.
    fn add_client(&mut self, id: u32) -> std::os::unix::net::UnixStream {
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        self.state.clients.insert(id, Self::client_state(a));
        b
    }

    /// The host xid the backend allocated for a core window resource.
    fn host_xid(&self, res: u32) -> u32 {
        self.state
            .resources
            .window(yserver_protocol::x11::ResourceId(res))
            .and_then(|w| w.host_xid)
            .expect("window has a host xid")
            .as_raw()
    }

    /// The WHOLE backing of a window's storage, ring included, through
    /// the privileged route — the only way anything can observe the
    /// ring, since step 3 confined every client route to content space.
    fn backing(&mut self, res: u32) -> (u32, u32, Vec<u8>) {
        let host = self.host_xid(res);
        self.backend
            .backing_pixels_for_tests(host)
            .expect("privileged backing read")
    }

    /// Dispatch one request. `body` excludes the 4-byte header.
    fn req(&mut self, opcode: u8, data: u8, body: &[u8]) {
        self.req_as(1, opcode, data, body);
    }

    /// [`Self::req`] from client `client`.
    fn req_as(&mut self, client: u32, opcode: u8, data: u8, body: &[u8]) {
        use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};
        assert!(
            body.len().is_multiple_of(4),
            "request bodies are 4-byte aligned"
        );
        self.seq = self.seq.wrapping_add(1);
        let header = RequestHeader {
            opcode,
            data,
            length_units: (body.len() / 4 + 1) as u32,
        };
        yserver_core::core_loop::process_request::process_request(
            &mut self.state,
            &mut self.backend,
            ClientId(client),
            SequenceNumber(self.seq),
            header,
            body,
            None,
        )
        .expect("process_request");
    }
}

fn or_create_window(
    f: &mut ProtoFixture,
    wid: u32,
    parent: u32,
    depth: u8,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    bw: u16,
    visual: u32,
    value_mask: u32,
    values: &[u32],
) {
    let mut body = Vec::new();
    body.extend_from_slice(&wid.to_le_bytes());
    body.extend_from_slice(&parent.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    body.extend_from_slice(&w.to_le_bytes());
    body.extend_from_slice(&h.to_le_bytes());
    body.extend_from_slice(&bw.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // class InputOutput
    body.extend_from_slice(&visual.to_le_bytes());
    body.extend_from_slice(&value_mask.to_le_bytes());
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    f.req(1, depth, &body);
}

fn or_create_gc(f: &mut ProtoFixture, gc: u32, drawable: u32, foreground: u32) {
    let mut body = Vec::new();
    body.extend_from_slice(&gc.to_le_bytes());
    body.extend_from_slice(&drawable.to_le_bytes());
    body.extend_from_slice(&0x0000_0004u32.to_le_bytes()); // GCForeground
    body.extend_from_slice(&foreground.to_le_bytes());
    f.req(55, 0, &body);
}

fn or_fill(f: &mut ProtoFixture, drawable: u32, gc: u32, x: i16, y: i16, w: u16, h: u16) {
    let mut body = Vec::new();
    body.extend_from_slice(&drawable.to_le_bytes());
    body.extend_from_slice(&gc.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    body.extend_from_slice(&w.to_le_bytes());
    body.extend_from_slice(&h.to_le_bytes());
    f.req(70, 0, &body);
}

/// The B, G, R bytes `GetImage` returns for an X11 pixel. Alpha is
/// deliberately excluded: at depth 24 the server owns it (it always
/// reads back `0xFF`, the L1 server-α invariant), and the depth-32
/// alpha rule has its own test.
fn or_bgr(pixel: u32) -> [u8; 3] {
    [
        (pixel & 0xFF) as u8,
        ((pixel >> 8) & 0xFF) as u8,
        ((pixel >> 16) & 0xFF) as u8,
    ]
}

// ───── #133 — the wezterm white-block regression (grow) ─────
//
// Structure, straight off the paired xtraces (`awesome.xtrace` line
// 4040-4078): awesome's frame gets `border_width = 16`, the client is
// reparented into it and configured to `(0, 17)` — 17 px below the
// frame's content origin for awesome's titlebar — and wezterm's GL
// child, the `PresentPixmap` target, fills the client. Then the whole
// thing GROWS (tile → maximise).
//
//     frame  0x0010006d  (x, y)  1136x1086  bw=16   →  1248x1391
//       client 0x00300003  (0, 17)  1136x1069  bw=0  →  1248x1374
//         GL   0x00300004  (0, 0)   1136x1069  bw=0  →  1248x1374
//
// Symptom: growing leaves the newly exposed region WHITE (uninitialised
// VRAM reads 0xFF on RADV); shrinking is always correct. That signature
// is "the walk samples more than has been written", so the assertion
// here is structural — placement versus the storage actually being
// sampled — not a pixel comparison, which a zeroed allocation would
// pass by luck.
fn wz_map(f: &mut ProtoFixture, wid: u32) {
    f.req(8, 0, &wid.to_le_bytes());
}

/// ConfigureWindow. `mask` bits: x=1, y=2, w=4, h=8, border-width=0x10.
fn wz_configure(f: &mut ProtoFixture, wid: u32, mask: u16, values: &[i32]) {
    let mut body = Vec::new();
    body.extend_from_slice(&wid.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    f.req(12, 0, &body);
}
