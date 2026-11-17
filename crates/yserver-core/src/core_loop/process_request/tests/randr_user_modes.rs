//! RANDR user modes: CreateMode, DestroyMode, AddOutputMode,
//! DeleteOutputMode and their projection into replies and events.
//!
//! Expected values come from Xorg 21.1.24 (`randr/rrmode.c`,
//! `randr/rroutput.c`, `randr/rrcrtc.c`) and a measured Xvfb run with a
//! connection held: duplicate or probed name BadName (errorValue the
//! window); AddOutputMode sends ScreenChangeNotify + OutputChangeNotify and
//! no CrtcChangeNotify, a second add nothing; DeleteOutputMode of a probed
//! mode or of a mode not on the output BadAccess; DestroyMode of a probed
//! mode BadMatch, of a listed mode BadAccess, errorValue the mode id.

use super::{
    access_resources::words,
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{AccessMode, Creation, Decision, ResourceKind, ResourceTarget},
    backend::{ModeSpec, UserTiming},
    randr::{ModeTiming, RandrMode, RandrOutput, RandrState},
    resources::SERVER_OWNER,
    server::RANDR_USER_MODE_XID_FIRST,
};
use yserver_protocol::x11::{randr as rr, request_swap::swap_request_body};

const VGA: u32 = 0x1_0001;
const VGA_CRTC: u32 = 0x1_0002;
const M1920X1200: u32 = 0x1_0003;
const M1024X768: u32 = 0x1_0004;
const HDMI: u32 = 0x1_0005;
const HDMI_CRTC: u32 = 0x1_0006;

const BAD_RR_OUTPUT: u8 = 147;
const BAD_RR_MODE: u8 = 149;
const SCREEN_CHANGE_NOTIFY: u8 = 89;
const RR_NOTIFY: u8 = 90;
const RR_CSYNC: u32 = 0x40;

fn output(
    name: &str,
    output_id: u32,
    crtc_id: u32,
    mode: Option<(u32, u16, u16)>,
    modes: &[u32],
) -> RandrOutput {
    let (mode_id, width, height) = mode.unwrap_or((0, 0, 0));
    RandrOutput {
        name: name.to_string(),
        output_id,
        crtc_id,
        mode_id,
        connected: true,
        x: 0,
        y: 0,
        width,
        height,
        vrefresh: if mode.is_some() { 60 } else { 0 },
        timing: None,
        mm_width: 520,
        mm_height: 320,
        mode_ids: modes.to_vec(),
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }
}

fn mode(mode_id: u32, width: u16, height: u16) -> RandrMode {
    RandrMode {
        mode_id,
        width,
        height,
        vrefresh: 60,
        timing: None,
    }
}

/// VGA-1 shows 1920x1200 and offers 1024x768 too; HDMI-1 is off and offers
/// 1024x768.
fn two_output_randr() -> RandrState {
    RandrState::from_outputs_with_modes(
        1,
        vec![
            output(
                "VGA-1",
                VGA,
                VGA_CRTC,
                Some((M1920X1200, 1920, 1200)),
                &[M1920X1200, M1024X768],
            ),
            output("HDMI-1", HDMI, HDMI_CRTC, None, &[M1024X768]),
        ],
        vec![mode(M1920X1200, 1920, 1200), mode(M1024X768, 1024, 768)],
    )
}

/// `cvt 1920 1080 60`: 173.00 1920 2048 2248 2576 1080 1083 1088 1120
/// -hsync +vsync.
fn cvt_1080() -> rr::ModeInfo {
    rr::ModeInfo {
        id: 0x4242,
        width: 1920,
        height: 1080,
        dot_clock: 173_000_000,
        hsync_start: 2048,
        hsync_end: 2248,
        htotal: 2576,
        hskew: 0,
        vsync_start: 1083,
        vsync_end: 1088,
        vtotal: 1120,
        name_len: 0,
        mode_flags: 0x6,
    }
}

struct Fixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
    order: ClientByteOrder,
    sequence: u16,
}

impl Fixture {
    fn new() -> Self {
        Self::with_order(ClientByteOrder::LittleEndian)
    }

    fn with_order(order: ClientByteOrder) -> Self {
        let mut state = ServerState::new();
        state.randr = two_output_randr();
        let peer = install_client(&mut state, 1);
        state.clients.get_mut(&1).unwrap().byte_order = order;
        Self {
            state,
            peer,
            backend: RecordingBackend::new(),
            order,
            sequence: 0,
        }
    }

    fn u16b(&self, v: u16) -> [u8; 2] {
        match self.order {
            ClientByteOrder::LittleEndian => v.to_le_bytes(),
            ClientByteOrder::BigEndian => v.to_be_bytes(),
        }
    }

    fn u32b(&self, v: u32) -> [u8; 4] {
        match self.order {
            ClientByteOrder::LittleEndian => v.to_le_bytes(),
            ClientByteOrder::BigEndian => v.to_be_bytes(),
        }
    }

    fn read_u16(&self, b: &[u8], at: usize) -> u16 {
        let bytes = b[at..at + 2].try_into().unwrap();
        match self.order {
            ClientByteOrder::LittleEndian => u16::from_le_bytes(bytes),
            ClientByteOrder::BigEndian => u16::from_be_bytes(bytes),
        }
    }

    fn read_u32(&self, b: &[u8], at: usize) -> u32 {
        let bytes = b[at..at + 4].try_into().unwrap();
        match self.order {
            ClientByteOrder::LittleEndian => u32::from_le_bytes(bytes),
            ClientByteOrder::BigEndian => u32::from_be_bytes(bytes),
        }
    }

    fn words(&self, values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|&v| self.u32b(v)).collect()
    }

    /// A body in the client's byte order, normalized by the reader's swap
    /// table as `client_reader` does, then dispatched with `length_units`.
    fn send_units(&mut self, minor: u8, mut body: Vec<u8>, length_units: u32) -> Vec<u8> {
        swap_request_body(128, minor, self.order, &mut body);
        self.sequence += 1;
        handle_randr_request(
            &mut self.state,
            &mut self.backend,
            ClientId(1),
            SequenceNumber(self.sequence),
            RequestHeader {
                opcode: 128,
                data: minor,
                length_units,
            },
            &body,
        )
        .expect("RANDR request");
        read_all_available(&mut self.peer)
    }

    fn send(&mut self, minor: u8, body: Vec<u8>) -> Vec<u8> {
        let units = u32::try_from(1 + body.len().div_ceil(4)).unwrap();
        self.send_units(minor, body, units)
    }

    fn create_mode_body(&self, window: u32, info: rr::ModeInfo, name: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.u32b(window));
        b.extend_from_slice(&self.u32b(info.id));
        b.extend_from_slice(&self.u16b(info.width));
        b.extend_from_slice(&self.u16b(info.height));
        b.extend_from_slice(&self.u32b(info.dot_clock));
        for v in [info.hsync_start, info.hsync_end, info.htotal, info.hskew] {
            b.extend_from_slice(&self.u16b(v));
        }
        for v in [info.vsync_start, info.vsync_end, info.vtotal] {
            b.extend_from_slice(&self.u16b(v));
        }
        b.extend_from_slice(&self.u16b(u16::try_from(name.len()).unwrap()));
        b.extend_from_slice(&self.u32b(info.mode_flags));
        b.extend_from_slice(name);
        b.resize(b.len().next_multiple_of(4), 0);
        b
    }

    fn create_mode_raw(&mut self, info: rr::ModeInfo, name: &[u8]) -> Vec<u8> {
        let body = self.create_mode_body(ROOT_WINDOW.0, info, name);
        self.send(rr::RR_CREATE_MODE, body)
    }

    /// CreateMode that must succeed; the reply's mode id.
    fn create_mode(&mut self, info: rr::ModeInfo, name: &[u8]) -> u32 {
        let reply = self.create_mode_raw(info, name);
        assert_eq!(reply.len(), 32, "one CreateMode reply: {reply:02x?}");
        assert_eq!(reply[0], 1, "reply, not error: {reply:02x?}");
        assert_eq!(self.read_u16(&reply, 2), self.sequence);
        assert_eq!(self.read_u32(&reply, 4), 0, "reply length");
        self.read_u32(&reply, 8)
    }

    fn destroy_mode(&mut self, mode: u32) -> Vec<u8> {
        let body = self.words(&[mode]);
        self.send(rr::RR_DESTROY_MODE, body)
    }

    fn add_output_mode(&mut self, output: u32, mode: u32) -> Vec<u8> {
        let body = self.words(&[output, mode]);
        self.send(rr::RR_ADD_OUTPUT_MODE, body)
    }

    fn delete_output_mode(&mut self, output: u32, mode: u32) -> Vec<u8> {
        let body = self.words(&[output, mode]);
        self.send(rr::RR_DELETE_OUTPUT_MODE, body)
    }

    /// `(code, errorValue, minor)` of the single error in `bytes`.
    fn error(&self, bytes: &[u8]) -> (u8, u32, u8) {
        assert_eq!(bytes.len(), 32, "one error: {bytes:02x?}");
        assert_eq!(bytes[0], 0, "an error: {bytes:02x?}");
        assert_eq!(bytes[10], 128, "major = RANDR");
        let minor = u8::try_from(self.read_u16(bytes, 8)).unwrap();
        (bytes[1], self.read_u32(bytes, 4), minor)
    }

    /// GetScreenResourcesCurrent: `(mode ids, names in order)`.
    fn screen_modes(&mut self) -> (Vec<u32>, Vec<Vec<u8>>, Vec<rr::ModeInfo>) {
        let body = self.words(&[ROOT_WINDOW.0]);
        let r = self.send(rr::RR_GET_SCREEN_RESOURCES_CURRENT, body);
        assert_eq!(r[0], 1, "GetScreenResourcesCurrent reply");
        let n_crtcs = usize::from(self.read_u16(&r, 16));
        let n_outputs = usize::from(self.read_u16(&r, 18));
        let n_modes = usize::from(self.read_u16(&r, 20));
        let mut at = 32 + 4 * (n_crtcs + n_outputs);
        let mut infos = Vec::new();
        for _ in 0..n_modes {
            let u16_at = |o: usize| self.read_u16(&r, at + o);
            infos.push(rr::ModeInfo {
                id: self.read_u32(&r, at),
                width: u16_at(4),
                height: u16_at(6),
                dot_clock: self.read_u32(&r, at + 8),
                hsync_start: u16_at(12),
                hsync_end: u16_at(14),
                htotal: u16_at(16),
                hskew: u16_at(18),
                vsync_start: u16_at(20),
                vsync_end: u16_at(22),
                vtotal: u16_at(24),
                name_len: u16_at(26),
                mode_flags: self.read_u32(&r, at + 28),
            });
            at += 32;
        }
        let mut names = Vec::new();
        for info in &infos {
            let len = usize::from(info.name_len);
            names.push(r[at..at + len].to_vec());
            at += len;
        }
        (infos.iter().map(|m| m.id).collect(), names, infos)
    }

    /// GetOutputInfo: `(modes, nPreferred)`.
    fn output_modes(&mut self, output: u32) -> (Vec<u32>, u16) {
        let body = self.words(&[output, 0]);
        let r = self.send(rr::RR_GET_OUTPUT_INFO, body);
        assert_eq!(r[0], 1, "GetOutputInfo reply");
        let n_crtcs = usize::from(self.read_u16(&r, 26));
        let n_modes = usize::from(self.read_u16(&r, 28));
        let n_preferred = self.read_u16(&r, 30);
        let at = 36 + 4 * n_crtcs;
        let modes = (0..n_modes)
            .map(|i| self.read_u32(&r, at + 4 * i))
            .collect();
        (modes, n_preferred)
    }

    fn select_all(&mut self) {
        let mask = rr::NOTIFY_MASK_SCREEN_CHANGE
            | rr::NOTIFY_MASK_CRTC_CHANGE
            | rr::NOTIFY_MASK_OUTPUT_CHANGE;
        self.state.randr_select_masks.insert((1, ROOT_WINDOW), mask);
    }

    fn set_crtc_config(&mut self, crtc: u32, mode: u32, outputs: &[u32]) -> Vec<u8> {
        let mut body = self.words(&[crtc, 0, 0]);
        body.extend_from_slice(&self.u16b(0));
        body.extend_from_slice(&self.u16b(0));
        body.extend_from_slice(&self.u32b(mode));
        body.extend_from_slice(&self.u16b(1));
        body.extend_from_slice(&[0, 0]);
        body.extend(self.words(outputs));
        self.send(rr::RR_SET_CRTC_CONFIG, body)
    }
}

#[test]
fn create_mode_replies_a_server_owned_id_from_the_user_range() {
    let mut f = Fixture::new();
    let id = f.create_mode(cvt_1080(), b"1920x1080_60.00");
    // FakeClientID(0): the request's id (0x4242) is ignored.
    assert_eq!(id, RANDR_USER_MODE_XID_FIRST);
    assert_eq!(
        f.state
            .access_resource_owner(ResourceId(id), ResourceKind::RandrMode),
        Some(SERVER_OWNER)
    );
    let stored = f.state.randr_user_modes.get(id).unwrap();
    assert_eq!(stored.name, b"1920x1080_60.00");
    assert_eq!(
        stored.info,
        rr::ModeInfo {
            id,
            name_len: 15,
            ..cvt_1080()
        }
    );
}

#[test]
fn create_mode_duplicate_or_probed_name_is_bad_name() {
    let mut f = Fixture::new();
    f.create_mode(cvt_1080(), b"foo");
    // Measured: same name and timings again, and a probed mode's name.
    for name in [&b"foo"[..], b"1920x1200", b"1024x768"] {
        let bytes = f.create_mode_raw(cvt_1080(), name);
        assert_eq!(
            f.error(&bytes),
            (x11::error::BAD_NAME, ROOT_WINDOW.0, rr::RR_CREATE_MODE),
            "{}",
            String::from_utf8_lossy(name)
        );
    }
    assert_eq!(f.state.randr_user_modes.modes().len(), 1);
}

#[test]
fn create_mode_same_timings_under_a_new_name_is_a_new_mode() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    let bar = f.create_mode(cvt_1080(), b"bar");
    assert_ne!(foo, bar);
    assert_eq!(f.state.randr_user_modes.modes().len(), 2);
}

#[test]
fn create_mode_length_and_window_errors() {
    let mut f = Fixture::new();
    // REQUEST_AT_LEAST_SIZE(xRRCreateModeReq): 40 bytes.
    let mut short = f.create_mode_body(ROOT_WINDOW.0, cvt_1080(), b"");
    short.truncate(32);
    let bytes = f.send(rr::RR_CREATE_MODE, short);
    assert_eq!(f.error(&bytes).0, x11::error::BAD_LENGTH);
    // nameLength past the request: BadLength after the window lookup.
    let body = f.create_mode_body(ROOT_WINDOW.0, cvt_1080(), b"abcd");
    let bytes = f.send_units(rr::RR_CREATE_MODE, body, 10);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_LENGTH, ROOT_WINDOW.0, rr::RR_CREATE_MODE)
    );
    // Unknown window: BadWindow before the name is looked at.
    let body = f.create_mode_body(0x00de_ad00, cvt_1080(), b"1920x1200");
    let bytes = f.send(rr::RR_CREATE_MODE, body);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_WINDOW, 0x00de_ad00, rr::RR_CREATE_MODE)
    );
    assert!(f.state.randr_user_modes.modes().is_empty());
}

#[test]
fn create_mode_out_of_ids_is_bad_alloc() {
    let mut f = Fixture::new();
    f.state.randr_user_modes = crate::randr::RandrUserModes::with_id_range(
        RANDR_USER_MODE_XID_FIRST,
        RANDR_USER_MODE_XID_FIRST,
    );
    f.create_mode(cvt_1080(), b"foo");
    let bytes = f.create_mode_raw(cvt_1080(), b"bar");
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_ALLOC, ROOT_WINDOW.0, rr::RR_CREATE_MODE)
    );
}

#[test]
fn destroy_mode_errors_follow_xorg() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    let bytes = f.destroy_mode(0x00de_ad02);
    assert_eq!(
        f.error(&bytes),
        (BAD_RR_MODE, 0x00de_ad02, rr::RR_DESTROY_MODE)
    );
    // A probed mode: BadMatch (`!mode->userScreen`).
    let bytes = f.destroy_mode(M1024X768);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_MATCH, M1024X768, rr::RR_DESTROY_MODE)
    );
    // On an output's list: BadAccess (`refcnt > 1`).
    assert!(f.add_output_mode(HDMI, foo).is_empty());
    let bytes = f.destroy_mode(foo);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_ACCESS, foo, rr::RR_DESTROY_MODE)
    );
    // REQUEST_SIZE_MATCH.
    let body = f.words(&[foo, 0]);
    let bytes = f.send(rr::RR_DESTROY_MODE, body);
    assert_eq!(f.error(&bytes).0, x11::error::BAD_LENGTH);
    // Off the list it goes, without events.
    assert!(f.delete_output_mode(HDMI, foo).is_empty());
    f.select_all();
    assert!(f.destroy_mode(foo).is_empty());
    assert!(f.state.randr_user_modes.get(foo).is_none());
    let bytes = f.destroy_mode(foo);
    assert_eq!(f.error(&bytes), (BAD_RR_MODE, foo, rr::RR_DESTROY_MODE));
}

#[test]
fn add_output_mode_errors_follow_xorg() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    let bytes = f.add_output_mode(0x00de_ad01, foo);
    assert_eq!(
        f.error(&bytes),
        (BAD_RR_OUTPUT, 0x00de_ad01, rr::RR_ADD_OUTPUT_MODE)
    );
    let bytes = f.add_output_mode(VGA, 0x00de_ad02);
    assert_eq!(
        f.error(&bytes),
        (BAD_RR_MODE, 0x00de_ad02, rr::RR_ADD_OUTPUT_MODE)
    );
    let body = f.words(&[VGA]);
    let bytes = f.send(rr::RR_ADD_OUTPUT_MODE, body);
    assert_eq!(f.error(&bytes).0, x11::error::BAD_LENGTH);
    assert!(f.state.randr_user_modes.output_list(VGA).is_empty());
}

#[test]
fn add_output_mode_sends_one_screen_and_one_output_event() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    // A server up for a while, so "now" is past the old config time.
    f.state.start_instant -= std::time::Duration::from_secs(5);
    f.state.randr.config_timestamp = 1;
    f.select_all();
    let events = f.add_output_mode(VGA, foo);
    assert_eq!(events.len(), 64, "two events: {events:02x?}");
    let (screen, output) = (&events[..32], &events[32..]);
    assert_eq!(screen[0], SCREEN_CHANGE_NOTIFY);
    assert_eq!((output[0], output[1]), (RR_NOTIFY, 1), "OutputChangeNotify");
    let config_timestamp = f.state.randr.config_timestamp;
    assert_ne!(config_timestamp, 1, "lastConfigTime moves");
    assert_eq!(f.read_u32(screen, 8), config_timestamp);
    assert_eq!(f.read_u32(output, 8), config_timestamp);
    assert_eq!(f.read_u32(output, 16), VGA, "output");
    assert_eq!(f.read_u32(output, 20), VGA_CRTC, "crtc");
    assert_eq!(f.read_u32(output, 24), M1920X1200, "the CRTC's mode");
    assert_eq!(f.state.randr_user_modes.output_list(VGA), &[foo]);

    // Already listed, user or probed: Success, nothing sent or moved.
    f.state.randr.config_timestamp = 1;
    assert!(f.add_output_mode(VGA, foo).is_empty());
    assert!(f.add_output_mode(VGA, M1024X768).is_empty());
    assert_eq!(f.state.randr.config_timestamp, 1);
    assert_eq!(f.state.randr_user_modes.output_list(VGA), &[foo]);
}

#[test]
fn delete_output_mode_errors_follow_xorg_and_send_nothing() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    let bar = f.create_mode(cvt_1080(), b"bar");
    assert!(f.add_output_mode(VGA, foo).is_empty());
    // A probed mode, and a user mode never added to this output: BadAccess.
    for mode in [M1920X1200, bar] {
        let bytes = f.delete_output_mode(VGA, mode);
        assert_eq!(
            f.error(&bytes),
            (x11::error::BAD_ACCESS, mode, rr::RR_DELETE_OUTPUT_MODE)
        );
    }
    let bytes = f.delete_output_mode(HDMI, foo);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_ACCESS, foo, rr::RR_DELETE_OUTPUT_MODE)
    );
    let bytes = f.delete_output_mode(0x00de_ad01, foo);
    assert_eq!(
        f.error(&bytes),
        (BAD_RR_OUTPUT, 0x00de_ad01, rr::RR_DELETE_OUTPUT_MODE)
    );
    f.select_all();
    f.state.randr.config_timestamp = 1;
    assert!(f.delete_output_mode(VGA, foo).is_empty());
    assert_eq!(f.state.randr.config_timestamp, 1, "no timestamp change");
    assert!(f.state.randr_user_modes.output_list(VGA).is_empty());
}

#[test]
fn delete_output_mode_of_the_crtcs_current_mode_is_bad_match() {
    // `output->crtc->mode == mode` (rroutput.c:254-255). A CRTC can show a
    // mode outside its output's probed list; on the user list it is the
    // current mode and stays.
    let mut f = Fixture::new();
    f.state.randr.outputs[0].mode_ids = vec![M1024X768];
    assert!(f.add_output_mode(VGA, M1920X1200).is_empty());
    let bytes = f.delete_output_mode(VGA, M1920X1200);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_MATCH, M1920X1200, rr::RR_DELETE_OUTPUT_MODE)
    );
    assert_eq!(f.state.randr_user_modes.output_list(VGA), &[M1920X1200]);
}

#[test]
fn screen_resources_list_modes_in_xorg_order_with_user_names() {
    let mut f = Fixture::new();
    let mut csync = cvt_1080();
    csync.hskew = 5;
    csync.mode_flags |= RR_CSYNC;
    let foo = f.create_mode(csync, b"foo");
    let bar = f.create_mode(cvt_1080(), b"bar");
    let baz = f.create_mode(cvt_1080(), b"baz");
    assert!(f.add_output_mode(HDMI, baz).is_empty());
    assert!(f.add_output_mode(HDMI, foo).is_empty());
    // RRModesForScreen: VGA probed, VGA user (none), HDMI probed (dup),
    // HDMI user in add order, CRTC modes (dup), then the rest by creation.
    let (ids, names, infos) = f.screen_modes();
    assert_eq!(ids, vec![M1920X1200, M1024X768, baz, foo, bar]);
    let names: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    assert_eq!(
        names,
        vec![&b"1920x1200"[..], b"1024x768", b"baz", b"foo", b"bar"]
    );
    // A user mode is echoed as sent: hSkew and every flag bit.
    assert_eq!(
        infos[3],
        rr::ModeInfo {
            id: foo,
            name_len: 3,
            ..csync
        }
    );
    // GetOutputInfo: probed, then user; nPreferred counts probed only.
    assert_eq!(f.output_modes(HDMI), (vec![M1024X768, baz, foo], 1));
    assert_eq!(f.output_modes(VGA), (vec![M1920X1200, M1024X768], 1));
}

#[test]
fn a_probed_mode_of_another_output_joins_the_user_list() {
    // RROutputAddUserMode does not require a user mode (rroutput.c:199-235).
    let mut f = Fixture::new();
    assert!(f.add_output_mode(HDMI, M1920X1200).is_empty());
    assert_eq!(f.output_modes(HDMI), (vec![M1024X768, M1920X1200], 1));
    assert!(f.delete_output_mode(HDMI, M1920X1200).is_empty());
    assert_eq!(f.output_modes(HDMI), (vec![M1024X768], 1));
}

#[test]
// A backend that cannot program a timing (nested, surface hosts for now).
fn set_crtc_config_to_a_user_mode_fails_without_reaching_the_backend() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    // Not on the output's lists: BadMatch.
    let bytes = f.set_crtc_config(VGA_CRTC, foo, &[VGA]);
    assert_eq!(f.error(&bytes).0, x11::error::BAD_MATCH);
    assert!(f.add_output_mode(VGA, foo).is_empty());
    let timestamp = f.state.randr.timestamp;
    let reply = f.set_crtc_config(VGA_CRTC, foo, &[VGA]);
    assert_eq!(reply.len(), 32, "{reply:02x?}");
    assert_eq!((reply[0], reply[1]), (1, rr::SET_CONFIG_FAILED));
    assert_eq!(f.read_u32(&reply, 8), timestamp);
    assert!(
        !f.backend
            .calls()
            .iter()
            .any(|call| matches!(call, RecordedCall::ApplyCrtcConfig { .. })),
        "the backend must not resolve a user mode by size"
    );
    assert_eq!(f.state.randr.outputs[0].mode_id, M1920X1200);
}

fn big_endian_outcome(order: ClientByteOrder) -> (u32, Vec<(u8, u32, u8)>, ServerSnapshot) {
    let mut f = Fixture::with_order(order);
    let mut info = cvt_1080();
    info.hskew = 5;
    let foo = f.create_mode(info, b"foo");
    let bar = f.create_mode(cvt_1080(), b"bar");
    let mut errors = Vec::new();
    assert!(f.add_output_mode(VGA, foo).is_empty());
    assert!(f.add_output_mode(HDMI, bar).is_empty());
    let bytes = f.destroy_mode(foo);
    errors.push(f.error(&bytes));
    let bytes = f.delete_output_mode(VGA, bar);
    errors.push(f.error(&bytes));
    assert!(f.delete_output_mode(HDMI, bar).is_empty());
    assert!(f.destroy_mode(bar).is_empty());
    let snapshot = ServerSnapshot {
        modes: f.state.randr_user_modes.modes().to_vec(),
        vga: f.state.randr_user_modes.output_list(VGA).to_vec(),
        hdmi: f.state.randr_user_modes.output_list(HDMI).to_vec(),
    };
    (foo, errors, snapshot)
}

#[derive(Debug, PartialEq, Eq)]
struct ServerSnapshot {
    modes: Vec<crate::randr::UserMode>,
    vga: Vec<u32>,
    hdmi: Vec<u32>,
}

#[test]
fn big_endian_clients_get_the_same_results_through_the_swap_table() {
    let (le_id, le_errors, le_state) = big_endian_outcome(ClientByteOrder::LittleEndian);
    let (be_id, be_errors, mut be_state) = big_endian_outcome(ClientByteOrder::BigEndian);
    assert_eq!(be_id, le_id);
    assert_eq!(be_errors, le_errors);
    assert_eq!(
        le_errors,
        vec![
            (x11::error::BAD_ACCESS, le_id, rr::RR_DESTROY_MODE),
            (x11::error::BAD_ACCESS, le_id + 1, rr::RR_DELETE_OUTPUT_MODE),
        ]
    );
    // SProcRRCreateMode does not swap hSkew; a big-endian client's skew
    // arrives byte-swapped, as on Xorg.
    assert_eq!(le_state.modes[0].info.hskew, 5);
    assert_eq!(be_state.modes[0].info.hskew, 0x0500);
    be_state.modes[0].info.hskew = 5;
    assert_eq!(be_state, le_state);
    assert_eq!(le_state.vga, vec![le_id]);
}

#[test]
fn user_modes_survive_a_backend_rebuild_and_a_hotplug() {
    let mut f = Fixture::new();
    f.backend.apply_crtc_configs = true;
    let foo = f.create_mode(cvt_1080(), b"foo");
    assert!(f.add_output_mode(HDMI, foo).is_empty());
    // A backend set rebuilds `state.randr` from the backend.
    let reply = f.set_crtc_config(VGA_CRTC, M1024X768, &[VGA]);
    let status = reply.chunks_exact(32).find(|c| c[0] == 1).map(|c| c[1]);
    assert_eq!(status, Some(rr::SET_CONFIG_SUCCESS), "{reply:02x?}");
    assert_eq!(f.state.randr.outputs[0].mode_id, M1024X768);
    assert_eq!(f.output_modes(HDMI).0, vec![M1024X768, foo]);
    // Unplug HDMI-1: not listed, its user list kept; replug brings it back.
    let vga_only = RandrState::from_outputs_with_modes(
        2,
        vec![f.state.randr.outputs[0].clone()],
        f.state.randr.mode_table.clone(),
    );
    f.state.randr = vga_only;
    let (ids, ..) = f.screen_modes();
    assert_eq!(
        ids,
        vec![M1920X1200, M1024X768, foo],
        "an unattached user mode stays"
    );
    assert_eq!(f.state.randr_user_modes.output_list(HDMI), &[foo]);
    f.state.randr = two_output_randr();
    assert_eq!(f.output_modes(HDMI).0, vec![M1024X768, foo]);
}

#[test]
fn user_modes_outlive_the_client_that_made_them() {
    let mut f = Fixture::new();
    let foo = f.create_mode(cvt_1080(), b"foo");
    assert!(f.add_output_mode(VGA, foo).is_empty());
    crate::core_loop::process_disconnect::process_disconnect(
        &mut f.state,
        &mut f.backend,
        ClientId(1),
    );
    assert!(f.state.randr_user_modes.get(foo).is_some());
    assert_eq!(f.state.randr_user_modes.output_list(VGA), &[foo]);
    let mut other = install_client(&mut f.state, 2);
    handle_randr_request(
        &mut f.state,
        &mut f.backend,
        ClientId(2),
        SequenceNumber(1),
        RequestHeader {
            opcode: 128,
            data: rr::RR_GET_OUTPUT_INFO,
            length_units: 3,
        },
        &words(&[VGA, 0]),
    )
    .expect("GetOutputInfo");
    let r = read_all_available(&mut other);
    let n_crtcs = usize::from(u16::from_le_bytes(r[26..28].try_into().unwrap()));
    let n_modes = usize::from(u16::from_le_bytes(r[28..30].try_into().unwrap()));
    let at = 36 + 4 * n_crtcs + 4 * (n_modes - 1);
    assert_eq!(u32::from_le_bytes(r[at..at + 4].try_into().unwrap()), foo);
}

fn recorded_resource_calls(
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
) -> Vec<(ResourceTarget, AccessMode, Option<ClientId>)> {
    let mut calls = calls.lock().unwrap();
    let out = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .map(|call| (call.target.unwrap(), call.mode.unwrap(), call.owner))
        .collect();
    calls.clear();
    out
}

fn plain(id: u32, kind: ResourceKind) -> ResourceTarget {
    ResourceTarget {
        id: ResourceId(id),
        kind,
        creation: None,
        owner: None,
    }
}

#[test]
fn a_recording_policy_sees_user_modes_as_server_owned() {
    let mut f = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    f.state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let server = Some(SERVER_OWNER);

    let foo = f.create_mode(cvt_1080(), b"foo");
    assert_eq!(
        recorded_resource_calls(&calls),
        vec![
            (
                plain(ROOT_WINDOW.0, ResourceKind::Window),
                AccessMode::GET_ATTR,
                server
            ),
            (
                ResourceTarget {
                    id: ResourceId(foo),
                    kind: ResourceKind::RandrMode,
                    creation: Some(Creation {
                        parent: None,
                        owner: SERVER_OWNER,
                    }),
                    owner: None,
                },
                AccessMode::CREATE,
                None,
            ),
        ]
    );

    assert!(f.add_output_mode(VGA, foo).is_empty());
    assert_eq!(
        recorded_resource_calls(&calls),
        vec![
            (
                plain(VGA, ResourceKind::RandrOutput),
                AccessMode::READ,
                server
            ),
            (plain(foo, ResourceKind::RandrMode), AccessMode::USE, server),
        ]
    );

    let _ = f.set_crtc_config(VGA_CRTC, foo, &[VGA]);
    let checks = recorded_resource_calls(&calls);
    assert!(
        checks.contains(&(
            plain(foo, ResourceKind::RandrMode),
            AccessMode::SET_ATTR,
            server
        )),
        "{checks:?}"
    );

    let _ = f.destroy_mode(foo);
    assert_eq!(
        recorded_resource_calls(&calls),
        vec![(
            plain(foo, ResourceKind::RandrMode),
            AccessMode::DESTROY,
            server
        )]
    );
}

#[test]
fn a_denied_create_stores_nothing() {
    let mut f = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denial = crate::access::DenyError {
        code: x11::error::BAD_ACCESS,
        value: 0xA55A_1234,
    };
    f.state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ResourceId(RANDR_USER_MODE_XID_FIRST),
            Some(AccessMode::CREATE),
            denial,
        )));
    let bytes = f.create_mode_raw(cvt_1080(), b"foo");
    assert_eq!(
        f.error(&bytes),
        (denial.code, denial.value, rr::RR_CREATE_MODE)
    );
    assert!(f.state.randr_user_modes.modes().is_empty());
}

// The CRTC's current mode: Xorg `xf86RandRModeMatches` over the output's
// probed list, then its user list (`xf86RandR12.c:996-1038`, `:1077-1088`).

/// The backend's id for VGA-1's programmed 1920x1080, a timing not in its
/// probed list.
const M1080_KERNEL: u32 = 0x1_0007;

/// `cvt_1080` as the kernel reports a programmed mode: clock in kHz.
fn cvt_1080_kernel() -> ModeTiming {
    ModeTiming {
        clock_khz: 173_000,
        hsync_start: 2048,
        hsync_end: 2248,
        htotal: 2576,
        hskew: 0,
        vsync_start: 1083,
        vsync_end: 1088,
        vtotal: 1120,
        mode_flags: 0x6,
    }
}

/// VGA-1's CRTC programmed with `timing` at 1920x1080 under
/// `M1080_KERNEL`; `probed` is VGA-1's probed list.
fn programmed(timing: ModeTiming, probed: &[u32]) -> Fixture {
    let mut f = Fixture::new();
    let mut vga = output(
        "VGA-1",
        VGA,
        VGA_CRTC,
        Some((M1080_KERNEL, 1920, 1080)),
        probed,
    );
    vga.timing = Some(timing);
    let current = RandrMode {
        mode_id: M1080_KERNEL,
        width: 1920,
        height: 1080,
        vrefresh: 60,
        timing: Some(timing),
    };
    f.state.randr = RandrState::from_outputs_with_modes(
        1,
        vec![vga, output("HDMI-1", HDMI, HDMI_CRTC, None, &[M1024X768])],
        vec![
            mode(M1920X1200, 1920, 1200),
            mode(M1024X768, 1024, 768),
            current,
        ],
    );
    f
}

impl Fixture {
    /// GetCrtcInfo's mode.
    fn crtc_mode(&mut self, crtc: u32) -> u32 {
        let body = self.words(&[crtc, 0]);
        let r = self.send(rr::RR_GET_CRTC_INFO, body);
        assert_eq!(r[0], 1, "GetCrtcInfo reply: {r:02x?}");
        self.read_u32(&r, 20)
    }

    /// The CRTC VGA-1 reports after adding a user mode with `info`.
    fn match_user_mode(&mut self, info: rr::ModeInfo) -> (u32, u32) {
        let id = self.create_mode(info, b"user");
        assert!(self.add_output_mode(VGA, id).is_empty());
        (id, self.crtc_mode(VGA_CRTC))
    }
}

#[test]
fn a_user_mode_with_the_programmed_timing_is_the_crtcs_mode_everywhere() {
    let mut f = programmed(cvt_1080_kernel(), &[M1920X1200, M1024X768]);
    // Not yet on VGA-1's lists: the backend id.
    assert_eq!(f.crtc_mode(VGA_CRTC), M1080_KERNEL);
    let foo = f.create_mode(cvt_1080(), b"foo");
    assert_eq!(
        f.crtc_mode(VGA_CRTC),
        M1080_KERNEL,
        "only listed modes match"
    );
    assert!(f.screen_modes().0.contains(&M1080_KERNEL));

    f.select_all();
    let events = f.add_output_mode(VGA, foo);
    assert_eq!(events.len(), 64, "{events:02x?}");
    assert_eq!(
        f.read_u32(&events[32..], 24),
        foo,
        "OutputChangeNotify mode"
    );
    assert_eq!(f.crtc_mode(VGA_CRTC), foo);
    let (ids, names, _) = f.screen_modes();
    assert_eq!(ids, vec![M1920X1200, M1024X768, foo]);
    assert_eq!(names[2], b"foo");

    // A backend change carrying the backend id reaches clients as the user
    // mode: CrtcChangeNotify and OutputChangeNotify.
    crate::core_loop::run::emit_randr_change_notifications(
        &mut f.state,
        &[(VGA, VGA_CRTC, M1080_KERNEL)],
    );
    let events = read_all_available(&mut f.peer);
    let notifies: Vec<&[u8]> = events
        .chunks_exact(32)
        .filter(|e| e[0] == RR_NOTIFY)
        .collect();
    let crtc = notifies
        .iter()
        .find(|e| e[1] == 0)
        .expect("CrtcChangeNotify");
    assert_eq!(f.read_u32(crtc, 12), VGA_CRTC);
    assert_eq!(f.read_u32(crtc, 16), foo, "CrtcChangeNotify mode");
    let out = notifies
        .iter()
        .find(|e| e[1] == 1)
        .expect("OutputChangeNotify");
    assert_eq!(f.read_u32(out, 24), foo, "OutputChangeNotify mode");

    // The backend id is gone: no such mode, its name is free again.
    let bytes = f.destroy_mode(M1080_KERNEL);
    assert_eq!(
        f.error(&bytes),
        (BAD_RR_MODE, M1080_KERNEL, rr::RR_DESTROY_MODE)
    );
    // The user mode is current: referenced, and not deletable from VGA-1.
    let bytes = f.destroy_mode(foo);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_ACCESS, foo, rr::RR_DESTROY_MODE)
    );
    let bytes = f.delete_output_mode(VGA, foo);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_MATCH, foo, rr::RR_DELETE_OUTPUT_MODE)
    );
}

#[test]
fn a_probed_mode_with_the_same_timing_wins_over_a_user_mode() {
    let mut f = programmed(cvt_1080_kernel(), &[M1080_KERNEL, M1920X1200]);
    let (foo, crtc) = f.match_user_mode(cvt_1080());
    assert_eq!(crtc, M1080_KERNEL);
    // VGA-1 probed then user, HDMI-1 probed.
    assert_eq!(
        f.screen_modes().0,
        vec![M1080_KERNEL, M1920X1200, foo, M1024X768]
    );
}

#[test]
fn hskew_or_a_flag_above_bit_five_is_part_of_the_match() {
    let mut skewed = cvt_1080();
    skewed.hskew = 1;
    assert_eq!(
        programmed(cvt_1080_kernel(), &[]).match_user_mode(skewed).1,
        M1080_KERNEL
    );
    let mut csync = cvt_1080();
    csync.mode_flags |= RR_CSYNC;
    assert_eq!(
        programmed(cvt_1080_kernel(), &[]).match_user_mode(csync).1,
        M1080_KERNEL
    );
    // Both programmed: they match.
    let mut kernel = cvt_1080_kernel();
    kernel.hskew = 1;
    kernel.mode_flags |= RR_CSYNC;
    let mut both = cvt_1080();
    both.hskew = 1;
    both.mode_flags |= RR_CSYNC;
    let (id, crtc) = programmed(kernel, &[]).match_user_mode(both);
    assert_eq!(crtc, id);
}

#[test]
fn flags_outside_flag_bits_are_ignored_by_the_match() {
    // `modeFlags & FLAG_BITS` on both sides; FLAG_BITS ends at
    // RR_ClockDivideBy2 (0x2000).
    let mut high = cvt_1080();
    high.mode_flags |= 0x4000;
    let (id, crtc) = programmed(cvt_1080_kernel(), &[]).match_user_mode(high);
    assert_eq!(crtc, id);
}

#[test]
fn the_dot_clock_matches_in_whole_khz() {
    // `dotClock / 1000 != Clock`: 173_000_400 and 173_000_900 Hz are both
    // 173000 kHz; 173_001_000 is not.
    for (dot_clock, matches) in [
        (173_000_400, true),
        (173_000_900, true),
        (172_999_999, false),
        (173_001_000, false),
    ] {
        let mut info = cvt_1080();
        info.dot_clock = dot_clock;
        let (id, crtc) = programmed(cvt_1080_kernel(), &[]).match_user_mode(info);
        let expected = if matches { id } else { M1080_KERNEL };
        assert_eq!(crtc, expected, "dotClock {dot_clock}");
    }
}

#[test]
fn the_first_matching_user_mode_in_add_order_wins() {
    let mut f = programmed(cvt_1080_kernel(), &[]);
    let mut a = cvt_1080();
    a.dot_clock = 173_000_400;
    let mut b = cvt_1080();
    b.dot_clock = 173_000_900;
    let a = f.create_mode(a, b"a");
    let b = f.create_mode(b, b"b");
    assert!(f.add_output_mode(VGA, b).is_empty());
    assert!(f.add_output_mode(VGA, a).is_empty());
    assert_eq!(f.crtc_mode(VGA_CRTC), b);
    // VGA-1's user list, HDMI-1's probed list, then the unreferenced
    // backend mode; never the replaced M1080_KERNEL.
    let (ids, ..) = f.screen_modes();
    assert_eq!(ids, vec![b, a, M1024X768, M1920X1200]);
}

// ── SetCrtcConfig programs a user mode (plan commit 3) ──────────────────

impl Fixture {
    /// A backend that programs user timings and reports them back.
    fn programming() -> Self {
        let mut f = Self::new();
        f.backend.program_user_mode_timings = true;
        f.backend.apply_crtc_configs = true;
        f
    }

    /// The mode of the one `ApplyCrtcConfig` the backend saw.
    fn applied_mode(&self) -> Option<ModeSpec> {
        let applied: Vec<Option<ModeSpec>> = self
            .backend
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::ApplyCrtcConfig { mode, .. } => Some(*mode),
                _ => None,
            })
            .collect();
        assert_eq!(applied.len(), 1, "{applied:?}");
        applied[0]
    }
}

#[test]
fn set_crtc_config_to_a_user_mode_programs_its_converted_timing() {
    let mut f = Fixture::programming();
    let mut info = cvt_1080();
    // Below one kHz, a skew, `RR_CSync` and a bit outside FLAG_BITS.
    info.dot_clock = 173_000_999;
    info.hskew = 7;
    info.mode_flags = 0x6 | RR_CSYNC | 0x4000;
    let foo = f.create_mode(info, b"foo");
    assert!(f.add_output_mode(VGA, foo).is_empty());
    f.select_all();

    let bytes = f.set_crtc_config(VGA_CRTC, foo, &[VGA]);
    let reply = bytes
        .chunks_exact(32)
        .find(|chunk| chunk[0] == 1)
        .expect("SetCrtcConfig reply");
    assert_eq!(reply[1], 0, "SetCrtcConfig succeeds");

    // xf86RandRModeConvert: Clock = dotClock / 1000, Flags & FLAG_BITS;
    // the kernel's refresh for 173000 kHz over 2576x1120 rounds to 60.
    let expected = UserTiming {
        width: 1920,
        height: 1080,
        timing: ModeTiming {
            clock_khz: 173_000,
            hsync_start: 2048,
            hsync_end: 2248,
            htotal: 2576,
            hskew: 7,
            vsync_start: 1083,
            vsync_end: 1088,
            vtotal: 1120,
            mode_flags: 0x6 | RR_CSYNC,
        },
    };
    assert_eq!(
        f.applied_mode(),
        Some(ModeSpec {
            width: 1920,
            height: 1080,
            vrefresh: 60,
            user_timing: Some(expected),
        })
    );

    // The backend reports the programmed timing under its own id; clients
    // see the user mode everywhere.
    let notifies: Vec<&[u8]> = bytes
        .chunks_exact(32)
        .filter(|e| e[0] == RR_NOTIFY)
        .collect();
    let crtc = notifies
        .iter()
        .find(|e| e[1] == 0)
        .expect("CrtcChangeNotify");
    assert_eq!(f.read_u32(crtc, 16), foo, "CrtcChangeNotify mode");
    let out = notifies
        .iter()
        .find(|e| e[1] == 1)
        .expect("OutputChangeNotify");
    assert_eq!(f.read_u32(out, 24), foo, "OutputChangeNotify mode");
    assert_eq!(f.crtc_mode(VGA_CRTC), foo);
    assert_ne!(f.state.randr.outputs[0].mode_id, foo, "a backend id");

    // Current: DestroyMode BadAccess, DeleteOutputMode BadMatch
    // (rrmode.c:332-347, rroutput.c:238-262).
    let bytes = f.destroy_mode(foo);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_ACCESS, foo, rr::RR_DESTROY_MODE)
    );
    let bytes = f.delete_output_mode(VGA, foo);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_MATCH, foo, rr::RR_DELETE_OUTPUT_MODE)
    );
    assert_eq!(f.crtc_mode(VGA_CRTC), foo);
}

#[test]
fn another_outputs_probed_mode_is_programmed_by_its_timing() {
    let mut f = Fixture::programming();
    // HDMI-1 does not list 1920x1200; AddOutputMode puts it on its user
    // list, and the backend gets the timing the replies advertise.
    assert!(f.add_output_mode(HDMI, M1920X1200).is_empty());
    let (_, _, infos) = f.screen_modes();
    let advertised = *infos.iter().find(|m| m.id == M1920X1200).unwrap();

    let bytes = f.set_crtc_config(HDMI_CRTC, M1920X1200, &[HDMI]);
    assert!(
        bytes
            .chunks_exact(32)
            .any(|chunk| chunk[0] == 1 && chunk[1] == 0),
        "SetCrtcConfig succeeds: {bytes:02x?}"
    );
    let user = f
        .applied_mode()
        .and_then(|mode| mode.user_timing)
        .expect("programmed by timing");
    assert_eq!((user.width, user.height), (1920, 1200));
    assert_eq!(user.timing.clock_khz, advertised.dot_clock / 1000);
    assert_eq!(
        (user.timing.htotal, user.timing.vtotal),
        (advertised.htotal, advertised.vtotal)
    );
}

#[test]
fn a_probed_mode_is_found_in_the_connector_list() {
    let mut f = Fixture::programming();
    let bytes = f.set_crtc_config(VGA_CRTC, M1024X768, &[VGA]);
    assert!(
        bytes
            .chunks_exact(32)
            .any(|chunk| chunk[0] == 1 && chunk[1] == 0),
        "SetCrtcConfig succeeds: {bytes:02x?}"
    );
    assert_eq!(
        f.applied_mode(),
        Some(ModeSpec {
            width: 1024,
            height: 768,
            vrefresh: 60,
            user_timing: None,
        })
    );
}

#[test]
fn user_timing_refresh_is_the_kernels() {
    // drm_mode_vrefresh: clock * 1000 / (htotal * vtotal), rounded, x2 for
    // interlace, /2 for doublescan. CEA 1080i60: 74250 kHz, 2200x1125.
    let mut user = UserTiming::from_mode_info(&cvt_1080());
    assert_eq!(user.vrefresh(), 60);
    user.timing = ModeTiming {
        clock_khz: 74_250,
        htotal: 2200,
        vtotal: 1125,
        mode_flags: 0x5 | 0x10,
        ..user.timing
    };
    assert_eq!(user.vrefresh(), 60);
    user.timing.mode_flags = 0x5 | 0x20;
    assert_eq!(user.vrefresh(), 15);
    user.timing.htotal = 0;
    assert_eq!(user.vrefresh(), 0);
}

// ── RANDR 1.0: GetScreenInfo and SetScreenConfig (plan commit 5) ────────
//
// RR10GetData (`rrscreen.c:664-737`) walks the output's probed modes then
// its user modes; one size per width×height in that order, one rate per
// RRVerticalRefresh, the first mode of a rate taking it. Measured on Xvfb
// with A, B, C and R below added to its single 1920x1200 output (whose
// one mode has a 0 clock, rate 0): sizes 1920x1200 [0, 50] and 1024x768
// [60, 75]; `--output --mode R` reports 1024x768 at 60 (R's own rate);
// SetScreenConfig 1024x768 at 75 programs C, at rate 0 A, 1920x1200 at 50
// B; each RRScreenChangeNotify carries the new size's index as sizeID.
// The fixture's probed modes are synthesized at 60 Hz instead.

/// A modeline: clock in kHz, h timings, v timings, flags.
fn modeline(clock_khz: u32, h: [u16; 4], v: [u16; 4], mode_flags: u32) -> rr::ModeInfo {
    rr::ModeInfo {
        id: 0,
        width: h[0],
        height: v[0],
        dot_clock: clock_khz * 1000,
        hsync_start: h[1],
        hsync_end: h[2],
        htotal: h[3],
        hskew: 0,
        vsync_start: v[1],
        vsync_end: v[2],
        vtotal: v[3],
        name_len: 0,
        mode_flags,
    }
}

/// `cvt 1024 768 60`: 59.92 Hz, rate 60.
fn cvt_768_60() -> rr::ModeInfo {
    modeline(63_500, [1024, 1072, 1176, 1328], [768, 771, 775, 798], 0x6)
}

/// `cvt 1920 1200 50`: 49.93 Hz, rate 50.
fn cvt_1200_50() -> rr::ModeInfo {
    modeline(
        158_250,
        [1920, 2040, 2240, 2560],
        [1200, 1203, 1209, 1238],
        0x6,
    )
}

/// `cvt 1024 768 75`: 74.90 Hz, rate 75.
fn cvt_768_75() -> rr::ModeInfo {
    modeline(82_000, [1024, 1088, 1192, 1360], [768, 771, 775, 805], 0x6)
}

/// `cvt -r 1024 768 60`: 59.87 Hz, rate 60, +hsync -vsync.
fn cvt_768_60_reduced() -> rr::ModeInfo {
    modeline(56_000, [1024, 1072, 1104, 1184], [768, 771, 775, 790], 0x9)
}

/// One RANDR 1.0 size: `(width, height, rates)`.
type Rr10SizeRates = (u16, u16, Vec<u16>);

impl Fixture {
    /// A, B, C and R created and added to VGA-1 in that order, for a
    /// RANDR 1.5 client.
    fn with_rr10_user_modes(mut self) -> (Self, [u32; 4]) {
        self.state.randr_client_versions.insert(ClientId(1), (1, 5));
        let mut ids = [0; 4];
        for (id, (info, name)) in ids.iter_mut().zip([
            (cvt_768_60(), &b"A"[..]),
            (cvt_1200_50(), b"B"),
            (cvt_768_75(), b"C"),
            (cvt_768_60_reduced(), b"R"),
        ]) {
            *id = self.create_mode(info, name);
            assert!(self.add_output_mode(VGA, *id).is_empty());
        }
        (self, ids)
    }

    /// GetScreenInfo: `(sizes with their rates, sizeID, rate)`.
    fn screen_info(&mut self) -> (Vec<Rr10SizeRates>, u16, u16) {
        let body = self.words(&[ROOT_WINDOW.0]);
        let r = self.send(rr::RR_GET_SCREEN_INFO, body);
        assert_eq!(r[0], 1, "GetScreenInfo reply: {r:02x?}");
        let n_sizes = usize::from(self.read_u16(&r, 20));
        let (size_id, rate) = (self.read_u16(&r, 22), self.read_u16(&r, 26));
        let mut rates_at = 32 + 8 * n_sizes;
        let mut sizes = Vec::new();
        for i in 0..n_sizes {
            let n_rates = usize::from(self.read_u16(&r, rates_at));
            let rates = (0..n_rates)
                .map(|j| self.read_u16(&r, rates_at + 2 + 2 * j))
                .collect();
            rates_at += 2 + 2 * n_rates;
            sizes.push((
                self.read_u16(&r, 32 + 8 * i),
                self.read_u16(&r, 34 + 8 * i),
                rates,
            ));
        }
        assert_eq!(r.len(), rates_at.next_multiple_of(4));
        (sizes, size_id, rate)
    }

    /// SetScreenConfig at `timestamp`, rotation normal: every message
    /// sent.
    fn set_screen_config(&mut self, timestamp: u32, size_id: u16, rate: u16) -> Vec<u8> {
        let config = self.state.randr.config_timestamp;
        let mut body = self.words(&[ROOT_WINDOW.0, timestamp, config]);
        for v in [size_id, 1, rate, 0] {
            body.extend_from_slice(&self.u16b(v));
        }
        self.send(rr::RR_SET_SCREEN_CONFIG, body)
    }

    /// The status of the one reply among `bytes`.
    fn reply_status(bytes: &[u8]) -> u8 {
        let replies: Vec<&[u8]> = bytes.chunks_exact(32).filter(|m| m[0] == 1).collect();
        assert_eq!(replies.len(), 1, "{bytes:02x?}");
        replies[0][1]
    }

    /// The modes of every `ApplyCrtcConfig` the backend saw.
    fn applied_modes(&self) -> Vec<Option<ModeSpec>> {
        self.backend
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::ApplyCrtcConfig { mode, .. } => Some(*mode),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn rr10_sizes_are_the_probed_then_the_user_modes_deduped_by_size_and_rate() {
    let (mut f, [a, b, c, r]) = Fixture::new().with_rr10_user_modes();
    let (sizes, size_id, rate) = f.screen_info();
    assert_eq!(
        sizes,
        vec![(1920, 1200, vec![60, 50]), (1024, 768, vec![60, 75])]
    );
    assert_eq!((size_id, rate), (0, 60));
    // Each rate is the first mode of it: the probed 1024x768, not A or R.
    let data = f.state.randr_view().rr10_data().unwrap();
    let pairs: Vec<Vec<(u16, u32)>> = data.sizes.iter().map(|s| s.rates.clone()).collect();
    assert_eq!(
        pairs,
        vec![
            vec![(60, M1920X1200), (50, b)],
            vec![(60, M1024X768), (75, c)]
        ]
    );
    // A user mode on another output is not in VGA-1's view.
    assert!(f.delete_output_mode(VGA, a).is_empty());
    assert!(f.delete_output_mode(VGA, r).is_empty());
    assert!(f.add_output_mode(HDMI, a).is_empty());
    assert_eq!(f.screen_info().0[1], (1024, 768, vec![60, 75]));
}

#[test]
fn rr10_current_size_and_rate_are_the_user_mode_the_crtc_reports() {
    let (mut f, [_, b, c, r]) = Fixture::programming().with_rr10_user_modes();
    // R shares its rate with the probed 1024x768 but is the current mode:
    // the rate is R's own (Xvfb `--output screen --mode R`: `*60`).
    for (mode, expected) in [(r, (1, 60)), (c, (1, 75)), (b, (0, 50))] {
        let bytes = f.set_crtc_config(VGA_CRTC, mode, &[VGA]);
        assert!(
            bytes.chunks_exact(32).any(|m| m[0] == 1 && m[1] == 0),
            "SetCrtcConfig succeeds: {bytes:02x?}"
        );
        assert_eq!(f.crtc_mode(VGA_CRTC), mode);
        let (_, size_id, rate) = f.screen_info();
        assert_eq!((size_id, rate), expected, "mode {mode:#x}");
    }
}

#[test]
fn set_screen_config_programs_the_user_mode_xorg_picks() {
    let (mut f, [_, b, c, _]) = Fixture::programming().with_rr10_user_modes();
    f.select_all();
    let user = |info: rr::ModeInfo| {
        let timing = UserTiming::from_mode_info(&info);
        Some(ModeSpec {
            width: info.width,
            height: info.height,
            vrefresh: timing.vrefresh(),
            user_timing: Some(timing),
        })
    };
    // 1024x768 at 75: C, by its timing.
    let bytes = f.set_screen_config(1000, 1, 75);
    assert_eq!(Fixture::reply_status(&bytes), 0, "SetScreenConfig succeeds");
    assert_eq!(
        f.applied_modes().last().copied().flatten(),
        user(cvt_768_75())
    );
    assert_eq!(f.crtc_mode(VGA_CRTC), c);
    assert_eq!(f.screen_info().1, 1);
    // RR10CurrentSizeID: the new screen size's index.
    let screen: Vec<&[u8]> = bytes
        .chunks_exact(32)
        .filter(|e| e[0] == SCREEN_CHANGE_NOTIFY)
        .collect();
    assert!(!screen.is_empty());
    assert!(screen.iter().all(|e| f.read_u16(e, 20) == 1), "sizeID");

    // Rate 0: the first rate of the size, the probed 1024x768 (Xvfb: A,
    // its first 1024x768 mode).
    let bytes = f.set_screen_config(1100, 1, 0);
    assert_eq!(Fixture::reply_status(&bytes), 0, "SetScreenConfig succeeds");
    let last = f.applied_modes().last().copied().flatten().unwrap();
    assert_eq!(
        (last.width, last.height, last.user_timing),
        (1024, 768, None)
    );
    assert_eq!(f.crtc_mode(VGA_CRTC), M1024X768);

    // 1920x1200 at 50: B, a size change through a user mode.
    let bytes = f.set_screen_config(1200, 0, 50);
    assert_eq!(Fixture::reply_status(&bytes), 0, "SetScreenConfig succeeds");
    assert_eq!(
        f.applied_modes().last().copied().flatten(),
        user(cvt_1200_50())
    );
    assert_eq!(f.crtc_mode(VGA_CRTC), b);
    assert_eq!(f.screen_info().1, 0);

    // Without B on the output, rate 50 is no rate of the size.
    let bytes = f.set_screen_config(1300, 0, 60);
    assert_eq!(Fixture::reply_status(&bytes), 0, "SetScreenConfig succeeds");
    assert!(f.delete_output_mode(VGA, b).is_empty());
    let bytes = f.set_screen_config(1400, 0, 50);
    assert_eq!(
        f.error(&bytes),
        (x11::error::BAD_VALUE, 50, rr::RR_SET_SCREEN_CONFIG)
    );
}

#[test]
fn set_screen_config_to_a_user_mode_fails_on_a_backend_without_timings() {
    let (mut f, _) = Fixture::new().with_rr10_user_modes();
    let timestamp = f.state.randr.timestamp;
    let bytes = f.set_screen_config(1000, 0, 50);
    assert_eq!(bytes.len(), 32, "{bytes:02x?}");
    assert_eq!((bytes[0], bytes[1]), (1, rr::SET_CONFIG_FAILED));
    assert_eq!(f.read_u32(&bytes, 8), timestamp, "lastSetTime unchanged");
    assert!(f.applied_modes().is_empty(), "the backend is not reached");
}

#[test]
fn rr10_current_size_id_is_ffff_without_a_size_of_the_screen() {
    let (mut f, _) = Fixture::new().with_rr10_user_modes();
    assert_eq!(f.state.randr_view().rr10_current_size_id(), 0);
    f.state.randr.screen_width = 1000;
    assert_eq!(f.state.randr_view().rr10_current_size_id(), 0xffff);
    f.state.randr.screen_width = 1024;
    f.state.randr.screen_height = 768;
    assert_eq!(f.state.randr_view().rr10_current_size_id(), 1);
}
