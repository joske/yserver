//! The bridge against a real core loop (the hardware-free KMS test backend),
//! with a second hand-written client as the other X party and a fake host
//! pasteboard. Expected values follow ICCCM §2.

use std::{
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use yserver_core::{
    core_loop::{self, CoreSender, Message, auth::AuthState, poll_tokens::ClientIdAllocator},
    server::ServerState,
    transport::Listener,
};

use super::{
    ClipboardBridge, Pasteboard,
    bridge::{POLL, latin1_to_string, string_to_latin1},
    conn::{
        ATOM, Conn, Event, INTEGER, PROPERTY_CHANGE_MASK, PROPERTY_NOTIFY, Property,
        SELECTION_CLEAR, SELECTION_NOTIFY, SELECTION_REQUEST, STRING, u32_at,
    },
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[derive(Default)]
struct FakeState {
    count: i64,
    text: Option<String>,
    writes: usize,
    polls: usize,
}

/// The host pasteboard; `set` is another host app copying.
#[derive(Clone, Default)]
struct FakePasteboard(Arc<Mutex<FakeState>>);

impl FakePasteboard {
    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.0.lock().unwrap()
    }

    fn set(&self, text: Option<&str>) {
        let mut s = self.state();
        s.count += 1;
        s.text = text.map(str::to_owned);
    }

    fn text(&self) -> Option<String> {
        self.state().text.clone()
    }

    fn writes(&self) -> usize {
        self.state().writes
    }

    fn polls(&self) -> usize {
        self.state().polls
    }
}

impl Pasteboard for FakePasteboard {
    fn change_count(&mut self) -> i64 {
        let mut s = self.state();
        s.polls += 1;
        s.count
    }

    fn read_text(&mut self) -> Option<String> {
        self.text()
    }

    fn write_text(&mut self, text: &str) -> i64 {
        let mut s = self.state();
        s.count += 1;
        s.writes += 1;
        s.text = Some(text.to_owned());
        s.count
    }
}

/// `run_core` on its own thread over a private unix socket; reports each
/// client's `internal` flag at shutdown.
struct Server {
    path: PathBuf,
    sender: CoreSender,
    handle: Option<JoinHandle<Vec<bool>>>,
}

impl Server {
    fn start() -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "yserver-clipboard-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let (poll, sender, rx) = core_loop::channel().unwrap();
        let core_sender = sender.clone_handle();
        let handle = std::thread::spawn(move || {
            let mut state = ServerState::new();
            let mut backend = crate::kms::render::KmsBackend::for_tests();
            // The fixture's placeholder DRM fd cannot be polled.
            backend.platform.devices.clear();
            core_loop::run_core(
                poll,
                rx,
                core_sender,
                &mut state,
                &mut backend,
                [Listener::Unix(listener)],
                &ClientIdAllocator::new(),
                AuthState::new(None),
                core_loop::ResetPolicy::NoReset,
                None,
            )
            .unwrap();
            let mut internal: Vec<bool> = state.clients.values().map(|c| c.internal).collect();
            internal.sort_unstable();
            assert_eq!(
                state.counted_clients(),
                internal.iter().filter(|i| !**i).count()
            );
            internal
        });
        Self {
            path,
            sender,
            handle: Some(handle),
        }
    }

    fn connect(&self) -> Conn {
        Conn::setup(UnixStream::connect(&self.path).unwrap()).unwrap()
    }

    /// A bridge on `pasteboard`, once it has set up its selection input.
    fn bridge(&self, pasteboard: &FakePasteboard) -> ClipboardBridge {
        let polls = pasteboard.polls();
        let bridge =
            ClipboardBridge::start(self.sender.clone_handle(), Box::new(pasteboard.clone()))
                .unwrap();
        wait_until("the bridge to set up", || pasteboard.polls() > polls);
        bridge
    }

    /// Shut down; the `internal` flags of the clients still connected.
    fn shutdown(mut self) -> Vec<bool> {
        self.sender.send(Message::Shutdown).unwrap();
        let internal = self.handle.take().unwrap().join().unwrap();
        let _ = std::fs::remove_file(&self.path);
        internal
    }
}

/// The other X party: a client with a window selecting PropertyChange.
struct Party {
    conn: Conn,
    window: u32,
    clipboard: u32,
    utf8: u32,
    /// The property conversions land in.
    property: u32,
    /// The property touched to learn the server time.
    time: u32,
}

impl Party {
    fn new(server: &Server) -> Self {
        let mut conn = server.connect();
        let window = conn.create_window(PROPERTY_CHANGE_MASK).unwrap();
        let clipboard = conn.intern_atom("CLIPBOARD").unwrap();
        let utf8 = conn.intern_atom("UTF8_STRING").unwrap();
        let property = conn.intern_atom("PARTY_PROPERTY").unwrap();
        let time = conn.intern_atom("PARTY_TIME").unwrap();
        conn.sync().unwrap();
        Self {
            conn,
            window,
            clipboard,
            utf8,
            property,
            time,
        }
    }

    fn atom(&mut self, name: &str) -> u32 {
        self.conn.intern_atom(name).unwrap()
    }

    /// The server time, from a zero-length append's PropertyNotify.
    fn time(&mut self) -> u32 {
        let (window, property) = (self.window, self.time);
        self.conn
            .change_property(2, window, property, STRING, 8, &[])
            .unwrap();
        let event = self
            .conn
            .wait_event(|e| is_property_notify(e, window, property, true))
            .unwrap();
        u32_at(&event, 12)
    }

    fn own(&mut self) -> u32 {
        let time = self.time();
        self.conn
            .set_selection_owner(self.window, self.clipboard, time)
            .unwrap();
        time
    }

    fn owner(&mut self) -> u32 {
        self.conn.get_selection_owner(self.clipboard).unwrap()
    }

    /// Wait for `window` to be destroyed. A disconnect is processed in one
    /// core step, so the window's client is gone too.
    fn wait_destroyed(&mut self, window: u32) {
        const BAD_WINDOW: u8 = 3;
        let property = self.time;
        wait_until("the window to be destroyed", || {
            self.conn
                .get_property(window, property, false)
                .unwrap()
                .is_err_and(|e| e.code == BAD_WINDOW)
        });
    }

    fn event(&mut self, code: u8) -> Event {
        self.conn.wait_event(|e| e[0] & 0x7f == code).unwrap()
    }

    /// Answer the next SelectionRequest with `data` as `type_`, or refuse
    /// with `None`. Returns the request.
    fn answer(&mut self, data: Option<(u32, &[u8])>) -> Event {
        let request = self.event(SELECTION_REQUEST);
        let (time, requestor, selection, target, property) = (
            u32_at(&request, 4),
            u32_at(&request, 12),
            u32_at(&request, 16),
            u32_at(&request, 20),
            u32_at(&request, 24),
        );
        let property = match data {
            Some((type_, bytes)) => {
                self.conn
                    .change_property(0, requestor, property, type_, 8, bytes)
                    .unwrap();
                property
            }
            None => 0,
        };
        self.conn
            .send_selection_notify(time, requestor, selection, target, property)
            .unwrap();
        request
    }

    /// Convert CLIPBOARD to `target`: the SelectionNotify's property and,
    /// unless it is None, the property's contents (deleted).
    fn convert(&mut self, target: u32) -> (u32, Option<Property>) {
        let time = self.time();
        let (window, clipboard, property) = (self.window, self.clipboard, self.property);
        self.conn
            .convert_selection(window, clipboard, target, property, time)
            .unwrap();
        let notify = self.event(SELECTION_NOTIFY);
        assert_eq!(u32_at(&notify, 4), time, "SelectionNotify time");
        assert_eq!(u32_at(&notify, 8), window, "requestor");
        assert_eq!(u32_at(&notify, 12), clipboard, "selection");
        assert_eq!(u32_at(&notify, 16), target, "target");
        let got = u32_at(&notify, 20);
        if got == 0 {
            return (0, None);
        }
        let data = self.conn.get_property(window, got, true).unwrap().unwrap();
        (got, Some(data))
    }
}

fn is_property_notify(e: &Event, window: u32, atom: u32, new_value: bool) -> bool {
    e[0] & 0x7f == PROPERTY_NOTIFY
        && u32_at(e, 4) == window
        && u32_at(e, 8) == atom
        && (e[16] == 0) == new_value
}

fn atoms_of(property: &Property) -> Vec<u32> {
    property
        .data
        .chunks_exact(4)
        .map(|c| u32_at(c, 0))
        .collect()
}

#[test]
fn latin1_round_trips_and_marks_what_it_cannot_hold() {
    assert_eq!(latin1_to_string(&[0x63, 0x61, 0x66, 0xe9]), "café");
    assert_eq!(
        string_to_latin1("café ☕"),
        [0x63, 0x61, 0x66, 0xe9, 0x20, b'?']
    );
}

#[test]
fn x_owner_text_reaches_the_host_once_without_echo() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);

    let owned_at = x.own();
    let request = x.answer(Some((x.utf8, "héllo ✓".as_bytes())));
    assert_eq!(u32_at(&request, 20), x.utf8, "UTF8_STRING first");
    assert_eq!(u32_at(&request, 16), x.clipboard);
    assert_eq!(u32_at(&request, 4), owned_at, "the owner's selection time");
    assert_ne!(u32_at(&request, 24), 0, "a property to write to");
    let bridge_window = u32_at(&request, 12);
    wait_until("the host text", || {
        pasteboard.text().as_deref() == Some("héllo ✓")
    });

    // Our own write is not a host change: the X owner keeps CLIPBOARD.
    std::thread::sleep(POLL * 3);
    assert_eq!(x.owner(), x.window);
    assert_eq!(pasteboard.writes(), 1);

    // The core learns of the bridge's close from the client's reader thread,
    // asynchronously; shutting down before it does would still count it.
    bridge.stop();
    x.wait_destroyed(bridge_window);
    drop(x);
    let internal = server.shutdown();
    assert!(internal.iter().all(|i| !i), "the bridge disconnected");
}

#[test]
fn an_owner_without_utf8_is_asked_for_latin1_string() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);

    x.own();
    let first = x.answer(None);
    assert_eq!(u32_at(&first, 20), x.utf8);
    let second = x.answer(Some((STRING, &[0x63, 0x61, 0x66, 0xe9])));
    assert_eq!(u32_at(&second, 20), STRING);
    wait_until("the host text", || {
        pasteboard.text().as_deref() == Some("café")
    });
    bridge.stop();
    server.shutdown();
}

#[test]
fn host_text_is_served_as_every_icccm_text_target() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    pasteboard.set(Some("tea ☕ café"));
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);
    wait_until("the bridge to own CLIPBOARD", || x.owner() != 0);

    let targets = x.atom("TARGETS");
    let (_, list) = x.convert(targets);
    let list = list.unwrap();
    assert_eq!((list.type_, list.format), (ATOM, 32));
    let mut got = atoms_of(&list);
    got.sort_unstable();
    let mut want = vec![
        targets,
        x.atom("MULTIPLE"),
        x.atom("TIMESTAMP"),
        x.utf8,
        STRING,
        x.atom("TEXT"),
    ];
    want.sort_unstable();
    assert_eq!(got, want);

    let (_, utf8) = x.convert(x.utf8);
    let utf8 = utf8.unwrap();
    assert_eq!((utf8.type_, utf8.format), (x.utf8, 8));
    assert_eq!(utf8.data, "tea ☕ café".as_bytes());

    let (_, latin1) = x.convert(STRING);
    let latin1 = latin1.unwrap();
    assert_eq!((latin1.type_, latin1.format), (STRING, 8));
    assert_eq!(latin1.data, b"tea ? caf\xe9");

    // TEXT: the owner's choice; STRING cannot hold the cup.
    let text = x.atom("TEXT");
    let (_, text) = x.convert(text);
    assert_eq!(text.unwrap().type_, x.utf8);

    let timestamp = x.atom("TIMESTAMP");
    let before = x.time();
    let (_, stamp) = x.convert(timestamp);
    let stamp = stamp.unwrap();
    assert_eq!(
        (stamp.type_, stamp.format, stamp.data.len()),
        (INTEGER, 32, 4)
    );
    assert!(u32_at(&stamp.data, 0) <= before, "acquired before now");

    let pixmap = x.atom("PIXMAP");
    assert_eq!(x.convert(pixmap), (0, None), "an unknown target is refused");

    assert_eq!(pasteboard.writes(), 0, "serving X never writes the host");
    bridge.stop();
    server.shutdown();
}

#[test]
fn multiple_converts_each_pair_and_nones_the_failed_target() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    pasteboard.set(Some("pair"));
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);
    wait_until("the bridge to own CLIPBOARD", || x.owner() != 0);

    let (multiple, atom_pair, pixmap) = (x.atom("MULTIPLE"), x.atom("ATOM_PAIR"), x.atom("PIXMAP"));
    let (first, second, pairs) = (x.atom("FIRST"), x.atom("SECOND"), x.atom("PAIRS"));
    let list: Vec<u8> = [x.utf8, first, pixmap, second]
        .iter()
        .flat_map(|a| a.to_le_bytes())
        .collect();
    let window = x.window;
    x.conn
        .change_property(0, window, pairs, atom_pair, 32, &list)
        .unwrap();
    let time = x.time();
    x.conn
        .convert_selection(window, x.clipboard, multiple, pairs, time)
        .unwrap();
    let notify = x.event(SELECTION_NOTIFY);
    assert_eq!(u32_at(&notify, 20), pairs);
    let result = x.conn.get_property(window, pairs, true).unwrap().unwrap();
    assert_eq!(result.type_, atom_pair);
    assert_eq!(atoms_of(&result), [x.utf8, first, 0, second]);
    let converted = x.conn.get_property(window, first, true).unwrap().unwrap();
    assert_eq!(converted.data, b"pair");
    bridge.stop();
    server.shutdown();
}

#[test]
fn large_host_text_goes_to_x_with_incr() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    let big = "0123456789abcdef".repeat(300 * 1024 / 16);
    let bridge = server.bridge(&pasteboard);
    pasteboard.set(Some(&big));
    let mut x = Party::new(&server);
    wait_until("the bridge to own CLIPBOARD", || x.owner() != 0);

    let incr = x.atom("INCR");
    let (property, start) = x.convert(x.utf8);
    let start = start.unwrap();
    // §2.7.2: type INCR, one 32-bit lower bound on the size.
    assert_eq!((start.type_, start.format), (incr, 32));
    assert!(u32_at(&start.data, 0) as usize <= big.len());
    let (window, mut got, mut chunks) = (x.window, Vec::new(), 0);
    loop {
        x.conn
            .wait_event(|e| is_property_notify(e, window, property, true))
            .unwrap();
        let chunk = x
            .conn
            .get_property(window, property, true)
            .unwrap()
            .unwrap();
        if chunk.type_ == 0 {
            // The INCR property's own NewValue, queued behind the notify.
            continue;
        }
        if chunk.data.is_empty() {
            break;
        }
        assert_eq!((chunk.type_, chunk.format), (x.utf8, 8));
        got.extend_from_slice(&chunk.data);
        chunks += 1;
    }
    assert!(chunks >= 2, "{chunks} chunks");
    assert_eq!(got, big.as_bytes());
    bridge.stop();
    server.shutdown();
}

#[test]
fn large_x_text_reaches_the_host_with_incr() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);
    // Two-byte characters; odd chunks split them.
    let big = "é".repeat(150 * 1024);
    let incr = x.atom("INCR");

    x.own();
    let request = x.event(SELECTION_REQUEST);
    let (time, requestor, target, property) = (
        u32_at(&request, 4),
        u32_at(&request, 12),
        u32_at(&request, 20),
        u32_at(&request, 24),
    );
    x.conn
        .select_input(requestor, PROPERTY_CHANGE_MASK)
        .unwrap();
    let size = u32::try_from(big.len()).unwrap();
    x.conn
        .change_property(0, requestor, property, incr, 32, &size.to_le_bytes())
        .unwrap();
    x.conn
        .send_selection_notify(time, requestor, x.clipboard, target, property)
        .unwrap();
    for chunk in big.as_bytes().chunks(100_001).chain([&[][..]]) {
        x.conn
            .wait_event(|e| is_property_notify(e, requestor, property, false))
            .unwrap();
        x.conn
            .change_property(0, requestor, property, x.utf8, 8, chunk)
            .unwrap();
    }
    wait_until("the host text", || {
        pasteboard.text().as_deref() == Some(big.as_str())
    });
    assert_eq!(pasteboard.writes(), 1);
    bridge.stop();
    server.shutdown();
}

#[test]
fn ownership_follows_the_latest_copy_on_either_side() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    pasteboard.set(Some("mac 1"));
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);
    wait_until("the bridge to own CLIPBOARD", || x.owner() != 0);
    let bridge_window = x.owner();

    // X copies: the host gets X's text.
    x.own();
    x.answer(Some((x.utf8, b"x 1")));
    wait_until("the host text", || {
        pasteboard.text().as_deref() == Some("x 1")
    });

    // The host copies: the bridge takes CLIPBOARD from X.
    pasteboard.set(Some("mac 2"));
    let clear = x.event(SELECTION_CLEAR);
    assert_eq!(u32_at(&clear, 8), x.window);
    assert_eq!(x.owner(), bridge_window);
    let (_, text) = x.convert(x.utf8);
    assert_eq!(text.unwrap().data, b"mac 2");

    // The host holds no text: the bridge lets CLIPBOARD go.
    pasteboard.set(None);
    wait_until("CLIPBOARD to have no owner", || x.owner() == 0);

    // X copies, then the owner quits: the host keeps the text, and so does
    // CLIPBOARD, now through the bridge.
    x.own();
    x.answer(Some((x.utf8, b"x 2")));
    wait_until("the host text", || {
        pasteboard.text().as_deref() == Some("x 2")
    });
    drop(x);
    let mut y = Party::new(&server);
    wait_until("the bridge to own CLIPBOARD again", || {
        y.owner() == bridge_window
    });
    let (_, text) = y.convert(y.utf8);
    assert_eq!(text.unwrap().data, b"x 2");

    bridge.stop();
    drop(y);
    server.shutdown();
}

#[test]
fn a_killed_bridge_reconnects_and_stays_internal() {
    let server = Server::start();
    let pasteboard = FakePasteboard::default();
    pasteboard.set(Some("again"));
    let bridge = server.bridge(&pasteboard);
    let mut x = Party::new(&server);
    wait_until("the bridge to own CLIPBOARD", || x.owner() != 0);
    let old = x.owner();

    let mut killer = server.connect();
    killer.kill_client(old).unwrap();
    killer.sync().unwrap();
    assert_eq!(x.owner(), 0, "the killed bridge's CLIPBOARD is gone");
    // The new connection may get the same resource base, and window.
    wait_until("a new bridge to own CLIPBOARD", || x.owner() != 0);
    let (_, text) = x.convert(x.utf8);
    assert_eq!(text.unwrap().data, b"again");

    // Shut down with everyone connected: the bridge is the internal one.
    let internal = server.shutdown();
    assert_eq!(internal, [false, false, true]);
    drop((x, killer, bridge));
}
