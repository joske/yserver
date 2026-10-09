use std::{
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use super::{Listener, ResetPolicy, ServerState, run_core};
use crate::{
    backend::recording::RecordingBackend,
    core_loop::{
        Generation, GenerationCounter, Message,
        auth::AuthState,
        poll_tokens::ClientIdAllocator,
        sender::{CoreSender, channel},
    },
    transport::Transport,
};
use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

/// Generous: these wait on real threads (setup, reader, core) under a
/// loaded test binary, and every use is a wait-for-success.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long a "must NOT happen" assertion watches for. The positive
/// cases below complete in single-digit milliseconds.
const QUIET: Duration = Duration::from_millis(400);

/// The value a `GenerationCounter` holds after `n` resets.
fn generation_after(n: u64) -> Generation {
    let counter = GenerationCounter::new();
    for _ in 0..n {
        counter.bump();
    }
    counter.current()
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// A `run_core` running on its own thread over a private unix socket.
struct Server {
    path: PathBuf,
    sender: CoreSender,
    generations: GenerationCounter,
    /// The loop's own id allocator, shared so a test can learn the
    /// `ClientId` the next connection will be given. Monotonic, so a
    /// peek before `establish()` names that client exactly.
    client_ids: std::sync::Arc<ClientIdAllocator>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl Server {
    fn start(policy: ResetPolicy, auth: std::sync::Arc<AuthState>) -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "yserver-reset-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let (poll, sender, rx) = channel().expect("channel");
        let generations = rx.generation_counter();
        let sender_for_core = sender.clone_handle();
        let client_ids = std::sync::Arc::new(ClientIdAllocator::new());
        let client_ids_for_core = client_ids.clone();
        let handle = std::thread::spawn(move || {
            let mut state = ServerState::new();
            let mut backend = RecordingBackend::new();
            run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                [Listener::Unix(listener)],
                &client_ids_for_core,
                auth,
                policy,
                None,
            )
        });
        Self {
            path,
            sender,
            generations,
            client_ids,
            handle: Some(handle),
        }
    }

    /// The id the next accepted connection will get.
    fn next_client_id(&self) -> ClientId {
        self.client_ids.peek()
    }

    fn with_policy(policy: ResetPolicy) -> Self {
        Self::start(policy, AuthState::new(None))
    }

    fn generation(&self) -> Generation {
        self.generations.current()
    }

    fn connect(&self) -> UnixStream {
        let peer = UnixStream::connect(&self.path).expect("connect");
        peer.set_read_timeout(Some(TIMEOUT)).expect("read timeout");
        peer
    }

    /// Connect and take the connection all the way to *established*:
    /// setup handshake, then a `GetInputFocus` round-trip. The reply
    /// is what proves the core reached `handle_client_setup_complete`
    /// — the setup reply alone is written by the setup thread and
    /// says nothing about `state.clients`.
    fn establish(&self) -> UnixStream {
        let mut peer = self.connect();
        peer.write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .expect("setup request");
        let mut header = [0_u8; 8];
        peer.read_exact(&mut header).expect("setup reply header");
        assert_eq!(header[0], 1, "setup must succeed");
        let len = usize::from(u16::from_le_bytes([header[6], header[7]])) * 4;
        peer.read_exact(&mut vec![0; len])
            .expect("setup reply body");
        round_trip(&mut peer);
        peer
    }

    fn shutdown(mut self) -> std::io::Result<()> {
        let _ = self.sender.send(Message::Shutdown);
        let result = self.handle.take().expect("handle").join().expect("join");
        let _ = std::fs::remove_file(&self.path);
        result
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.sender.send(Message::Shutdown);
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `GetInputFocus` — the classic X11 round-trip probe. Always replies,
/// changes nothing.
fn round_trip(peer: &mut UnixStream) {
    peer.write_all(&[43, 0, 1, 0]).expect("GetInputFocus");
    let mut reply = [0_u8; 32];
    peer.read_exact(&mut reply).expect("GetInputFocus reply");
    assert_eq!(reply[0], 1, "reply, not an error");
}

/// `SetCloseDownMode(RetainPermanent)` — opcode 112, mode in the data
/// byte. Followed by a round-trip so the request is known to have been
/// processed before the caller drops the socket.
fn set_retain_permanent(peer: &mut UnixStream) {
    peer.write_all(&[112, 1, 1, 0]).expect("SetCloseDownMode");
    round_trip(peer);
}

// -- the trigger table: last client leaves x each policy ----------

#[test]
fn the_last_client_leaving_resets_under_reset() {
    let server = Server::with_policy(ResetPolicy::Reset);
    let peer = server.establish();
    assert_eq!(server.generation(), generation_after(0));
    drop(peer);
    wait_until("the drained session to reset", || {
        server.generation() == generation_after(1)
    });
    server.shutdown().expect("clean shutdown");
}

#[test]
fn the_last_client_leaving_does_nothing_under_noreset() {
    let server = Server::with_policy(ResetPolicy::NoReset);
    let peer = server.establish();
    drop(peer);
    std::thread::sleep(QUIET);
    // Non-vacuous: the server is still serving, so the loop did run
    // through the disconnect — it simply did not reset.
    let survivor = server.establish();
    assert_eq!(
        server.generation(),
        generation_after(0),
        "-noreset must never cross the boundary"
    );
    drop(survivor);
    server.shutdown().expect("clean shutdown");
}

#[test]
fn the_last_client_leaving_terminates_under_terminate() {
    let mut server = Server::with_policy(ResetPolicy::Terminate);
    let peer = server.establish();
    drop(peer);
    let handle = server.handle.take().expect("handle");
    wait_until("run_core to return", || handle.is_finished());
    handle
        .join()
        .expect("join")
        .expect("-terminate must shut down cleanly, not error");
    assert_eq!(
        server.generation(),
        generation_after(0),
        "-terminate exits instead of resetting"
    );
}

#[test]
fn a_retain_permanent_client_does_not_inhibit_the_reset() {
    // `process_disconnect` keeps a retained client's resources as a
    // zombie but removes it from `state.clients` regardless, so the
    // session still counts as drained. Xorg's `really_close_down`
    // gate does the opposite; the forced teardown at the boundary is
    // what makes ours safe.
    let server = Server::with_policy(ResetPolicy::Reset);
    let mut peer = server.establish();
    set_retain_permanent(&mut peer);
    drop(peer);
    wait_until("a retained client's departure to reset", || {
        server.generation() == generation_after(1)
    });
    server.shutdown().expect("clean shutdown");
}

#[test]
fn a_client_that_leaves_while_another_stays_does_not_reset() {
    let server = Server::with_policy(ResetPolicy::Reset);
    let first = server.establish();
    let mut second = server.establish();
    drop(first);
    std::thread::sleep(QUIET);
    round_trip(&mut second);
    assert_eq!(
        server.generation(),
        generation_after(0),
        "the session is not drained while a client remains"
    );
    drop(second);
    wait_until("the second departure to reset", || {
        server.generation() == generation_after(1)
    });
    server.shutdown().expect("clean shutdown");
}

// -- SIGHUP, per policy -------------------------------------------

#[test]
fn sighup_resets_a_running_session_under_reset() {
    let server = Server::with_policy(ResetPolicy::Reset);
    let _peer = server.establish();
    server
        .sender
        .send(Message::ResetRequested)
        .expect("send SIGHUP request");
    wait_until("SIGHUP to reset", || {
        server.generation() == generation_after(1)
    });
    server.shutdown().expect("clean shutdown");
}

#[test]
fn sighup_resets_rather_than_terminating_under_terminate() {
    let server = Server::with_policy(ResetPolicy::Terminate);
    let _peer = server.establish();
    server
        .sender
        .send(Message::ResetRequested)
        .expect("send SIGHUP request");
    wait_until("SIGHUP to reset", || {
        server.generation() == generation_after(1)
    });
    assert!(
        !server.handle.as_ref().expect("handle").is_finished(),
        "SIGHUP raises DE_RESET, not DE_TERMINATE"
    );
    server.shutdown().expect("clean shutdown");
}

#[test]
fn sighup_does_not_reset_under_noreset() {
    // Under the default policy the signal thread never produces this
    // message (it keeps sending `Shutdown`); the loop refuses it
    // anyway, so `-noreset` behaviour cannot drift.
    let server = Server::with_policy(ResetPolicy::NoReset);
    let mut peer = server.establish();
    server
        .sender
        .send(Message::ResetRequested)
        .expect("send SIGHUP request");
    std::thread::sleep(QUIET);
    round_trip(&mut peer);
    assert_eq!(server.generation(), generation_after(0));
    drop(peer);
    server.shutdown().expect("clean shutdown");
}

// -- the arming cases a happy-path suite misses --------------------

#[test]
fn an_idle_reset_server_never_resets() {
    // No client has ever connected, so the client set is empty from
    // the first iteration. A state check would reset here, over and
    // over.
    let server = Server::with_policy(ResetPolicy::Reset);
    std::thread::sleep(QUIET);
    assert_eq!(server.generation(), generation_after(0));
    // Still serving: the idle loop was running, not wedged.
    let peer = server.establish();
    assert_eq!(server.generation(), generation_after(0));
    drop(peer);
    server.shutdown().expect("clean shutdown");
}

#[test]
fn a_connection_dropped_before_setup_completes_arms_nothing() {
    // Accepted, a setup thread spawned, then gone before any
    // `ClientSetupComplete`. Nothing was ever established, so
    // nothing may fire.
    let server = Server::with_policy(ResetPolicy::Reset);
    for _ in 0..5 {
        let peer = server.connect();
        drop(peer);
    }
    std::thread::sleep(QUIET);
    assert_eq!(
        server.generation(),
        generation_after(0),
        "accept is not arming"
    );
    let peer = server.establish();
    assert_eq!(server.generation(), generation_after(0));
    drop(peer);
    server.shutdown().expect("clean shutdown");
}

#[test]
fn a_client_refused_for_a_bad_cookie_arms_nothing() {
    // The case a stranger can reach: stage 1's TCP listener binds
    // 0.0.0.0, so a wrong cookie must not be able to erase a
    // session. Refused inside the setup thread — the core never
    // hears about it.
    let cookie = [0x5A_u8; 16];
    let auth_path = write_xauth(&cookie);
    let server = Server::start(ResetPolicy::Reset, AuthState::new(Some(auth_path.clone())));

    let mut peer = server.connect();
    write_setup_with_cookie(&mut peer, &[0x22_u8; 16]);
    let mut header = [0_u8; 8];
    peer.read_exact(&mut header).expect("setup reply header");
    assert_eq!(header[0], 0, "a bad cookie must be refused");
    drop(peer);

    std::thread::sleep(QUIET);
    assert_eq!(
        server.generation(),
        generation_after(0),
        "a refused connection is not arming"
    );

    // And the good cookie still works, so the refusal was the
    // server's decision, not a broken fixture.
    let mut good = server.connect();
    write_setup_with_cookie(&mut good, &cookie);
    good.read_exact(&mut header).expect("setup reply header");
    assert_eq!(header[0], 1, "the matching cookie must be accepted");
    let len = usize::from(u16::from_le_bytes([header[6], header[7]])) * 4;
    good.read_exact(&mut vec![0; len])
        .expect("setup reply body");
    round_trip(&mut good);
    assert_eq!(server.generation(), generation_after(0));

    drop(good);
    wait_until("the authorized client's departure to reset", || {
        server.generation() == generation_after(1)
    });
    server.shutdown().expect("clean shutdown");
    let _ = std::fs::remove_file(&auth_path);
}

#[test]
fn a_reset_disarms_the_new_generation() {
    // The boundary leaves an empty client set behind, and the dead
    // generation's reader thread posts its `ClientDisconnected`
    // afterwards. Neither may produce a second reset.
    let server = Server::with_policy(ResetPolicy::Reset);
    let peer = server.establish();
    drop(peer);
    wait_until("the first reset", || {
        server.generation() == generation_after(1)
    });
    std::thread::sleep(QUIET);
    assert_eq!(
        server.generation(),
        generation_after(1),
        "exactly one reset; the fresh generation starts disarmed"
    );
    // The new generation serves, and arms again on its own client.
    let peer = server.establish();
    drop(peer);
    wait_until("the second generation to reset in turn", || {
        server.generation() == generation_after(2)
    });
    server.shutdown().expect("clean shutdown");
}

// -- the generation quarantine: producers bound at creation time ---

/// Watch a socket for `QUIET` and report whether the server wrote
/// **no bytes** to it. Restores the long timeout, so the caller can
/// keep using the socket afterwards.
///
/// "The far end went away without writing" counts as quiet, and has
/// three shapes here: a timeout (nobody holds the other half open),
/// `Ok(0)`, and — when the other half is dropped while bytes we sent
/// are still unread in its queue, which is exactly what discarding a
/// message carrying a `Transport` does — `ECONNRESET`. What must not
/// happen is bytes arriving; a caller that also cares whether the
/// peer is still *alive* follows this with a `round_trip`.
fn stays_quiet(peer: &mut UnixStream) -> bool {
    peer.set_read_timeout(Some(QUIET)).expect("read timeout");
    let mut byte = [0_u8; 1];
    let quiet = match peer.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(err) => matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ),
    };
    peer.set_read_timeout(Some(TIMEOUT)).expect("read timeout");
    quiet
}

/// The hole the quarantine exists to close: a producer belonging to
/// the destroyed session must not be able to hand the new one a
/// client.
///
/// `reset_generation` shuts the setup sockets down, which narrows the
/// window but does not close it — a setup thread can already hold a
/// fully decoded `ClientSetupComplete` and be one instruction away
/// from sending it. The producer here stands in for that thread: it
/// takes its handle while generation 0 runs, exactly where
/// `accept_pending` hands one to `setup_thread::spawn`, and sends
/// after the boundary. Tagging at *send* time would stamp it with the
/// new generation and let it through.
#[test]
fn an_old_setup_completion_cannot_create_a_client_in_the_new_generation() {
    let server = Server::with_policy(ResetPolicy::Reset);
    // The id the pre-reset client holds — what a real old setup
    // thread's completion would carry.
    let doomed = server.next_client_id();
    let stale = server.sender.bind();
    let peer = server.establish();
    assert_eq!(server.generation(), generation_after(0));
    drop(peer);
    wait_until("the drained session to reset", || {
        server.generation() == generation_after(1)
    });

    let (core_side, mut phantom) = UnixStream::pair().expect("socketpair");
    stale
        .send(Message::ClientSetupComplete {
            id: doomed,
            generation: stale.generation(),
            stream: Transport::Unix(core_side),
            resource_id_base: 0x0020_0000,
            resource_id_mask: 0x000F_FFFF,
            byte_order: ClientByteOrder::LittleEndian,
            is_local: true,
            fd_passing: true,
            setup_reply: Vec::new(),
        })
        .expect("send the stale completion");

    // Accepted, this would insert a `ClientState` and spawn a reader,
    // and the request below would come back answered.
    phantom.write_all(&[43, 0, 1, 0]).expect("GetInputFocus");
    assert!(
        stays_quiet(&mut phantom),
        "a client authorized in the destroyed session must not be served by the new one"
    );
    // Nor may it arm the fresh generation: an accepted completion
    // calls `note_client_established`, and the phantom's own
    // departure would then reset a session it was never part of.
    drop(phantom);
    std::thread::sleep(QUIET);
    assert_eq!(
        server.generation(),
        generation_after(1),
        "the phantom must not arm — and then drain — the new generation"
    );
    server.shutdown().expect("clean shutdown");
}

/// The reader-thread half: a `Request` produced by a retired reader
/// must not execute against the new session.
///
/// It names a client of the *new* session deliberately. The loop
/// already drops a request whose client is unknown
/// (`process_request_inline`'s post-disconnect guard), so an old id
/// would pass whether or not the generation filter works, and the
/// test would prove nothing. The reply landing on a live client's
/// socket is the sharpest observable there is.
#[test]
fn an_old_generation_request_is_not_executed_in_the_new_session() {
    let server = Server::with_policy(ResetPolicy::Reset);
    let stale = server.sender.bind();
    let peer = server.establish();
    drop(peer);
    wait_until("the drained session to reset", || {
        server.generation() == generation_after(1)
    });

    let live_id = server.next_client_id();
    let mut live = server.establish();

    stale
        .send(Message::Request {
            id: live_id,
            sequence: SequenceNumber(0x4242),
            accepted_at: None,
            header: RequestHeader {
                opcode: 43, // GetInputFocus — always replies
                data: 0,
                length_units: 1,
            },
            body: Vec::new(),
            attached_fd: None,
        })
        .expect("send the stale request");

    assert!(
        stays_quiet(&mut live),
        "a request tagged by a retired producer must not be executed"
    );
    // Quiet because the request was discarded, not because the
    // client is broken.
    round_trip(&mut live);
    assert_eq!(server.generation(), generation_after(1));
    drop(live);
    server.shutdown().expect("clean shutdown");
}

/// The other message a retired reader can still emit. Accepted, it
/// runs `disconnect_with_pending_cleanup` against the new session —
/// which under `-reset` drains it and resets a generation that was
/// serving a live client.
#[test]
fn an_old_generation_disconnect_cannot_tear_down_a_new_session_client() {
    let server = Server::with_policy(ResetPolicy::Reset);
    let stale = server.sender.bind();
    let peer = server.establish();
    drop(peer);
    wait_until("the drained session to reset", || {
        server.generation() == generation_after(1)
    });

    let live_id = server.next_client_id();
    let mut live = server.establish();

    stale
        .send(Message::ClientDisconnected {
            id: live_id,
            reason: std::io::Error::other("retired reader"),
        })
        .expect("send the stale disconnect");

    std::thread::sleep(QUIET);
    assert_eq!(
        server.generation(),
        generation_after(1),
        "a retired producer must not be able to drain the new session"
    );
    round_trip(&mut live);

    // The real departure still works, so the filter did not wedge
    // the trigger.
    drop(live);
    wait_until("the live client's own departure to reset", || {
        server.generation() == generation_after(2)
    });
    server.shutdown().expect("clean shutdown");
}

/// The other half of binding at accept: a connection accepted *after*
/// the boundary gets a producer bound to the new generation, so
/// nothing it sends is stale. Both its setup thread and its reader
/// thread have to pass the filter for `establish` (which ends in a
/// `GetInputFocus` round-trip) to return at all.
#[test]
fn a_connection_accepted_after_a_reset_is_served_normally() {
    let server = Server::with_policy(ResetPolicy::Reset);
    let peer = server.establish();
    drop(peer);
    wait_until("the drained session to reset", || {
        server.generation() == generation_after(1)
    });

    let mut fresh = server.establish();
    for _ in 0..3 {
        round_trip(&mut fresh);
    }
    assert_eq!(server.generation(), generation_after(1));
    drop(fresh);
    wait_until("the second generation to reset in turn", || {
        server.generation() == generation_after(2)
    });
    server.shutdown().expect("clean shutdown");
}

// -- fixtures for the auth case ------------------------------------

const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

fn write_xauth(cookie: &[u8]) -> PathBuf {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&256_u16.to_be_bytes()); // FamilyLocal
    for field in [
        b"host".as_slice(),
        b"0",
        MIT_MAGIC_COOKIE.as_bytes(),
        cookie,
    ] {
        bytes.extend_from_slice(
            &u16::try_from(field.len())
                .expect("field fits")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(field);
    }
    let path = std::env::temp_dir().join(format!("yserver-reset-auth-{}", std::process::id()));
    std::fs::write(&path, bytes).expect("write xauthority");
    path
}

fn write_setup_with_cookie(peer: &mut UnixStream, cookie: &[u8]) {
    let name = MIT_MAGIC_COOKIE.as_bytes();
    let mut buf = Vec::new();
    buf.push(b'l');
    buf.push(0);
    buf.extend_from_slice(&11_u16.to_le_bytes());
    buf.extend_from_slice(&0_u16.to_le_bytes());
    buf.extend_from_slice(&u16::try_from(name.len()).expect("name fits").to_le_bytes());
    buf.extend_from_slice(
        &u16::try_from(cookie.len())
            .expect("cookie fits")
            .to_le_bytes(),
    );
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(name);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
    buf.extend_from_slice(cookie);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
    peer.write_all(&buf).expect("setup request with cookie");
}
