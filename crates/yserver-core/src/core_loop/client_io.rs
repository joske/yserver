//! Per-client outbound write helper.
//!
//! The core thread hands buffers (event/reply bytes) to
//! `write_or_buffer`. It tries a direct non-blocking `write(2)` first;
//! on `EAGAIN`/partial write it buffers the remainder on
//! `ClientState::outbound` and returns `WouldBlock` so the caller can
//! arrange `WRITABLE` interest in the poller (I2). The buffer grows as
//! Xorg's does (`FlushClient`, `os/io.c`), up to `INT_MAX`; only past
//! that do we return `Disconnect`.
//!
//! Every failed write also marks the client `write_failed`: later writes
//! to it are refused without touching the socket, and the core loop
//! disconnects it through the normal path (`failed_writers`). Callers may
//! act on the outcome, but ignoring it can no longer keep the client.
//!
//! Today's `ClientState::writer` is still `Arc<Mutex<UnixStream>>`
//! (A1 left it untouched; D2 demotes it). The helper transparently
//! locks the mutex; once D2 lands we strip the lock without touching
//! call sites.

use std::{
    collections::VecDeque,
    io::{self, ErrorKind, Write},
};

use yserver_protocol::x11::ClientId;

use crate::{
    fd::{OwnedFd, RawFd},
    server::ClientState,
    transport::Transport,
};

/// Maximum bytes queued per client before we treat the peer as
/// unrecoverably slow and disconnect: Xorg's, whose `FlushClient`
/// (`os/io.c`) grows a client's output buffer for as long as
/// `notWritten + BUFSIZE <= INT_MAX` and aborts the client only past it.
///
/// It was 4 MiB, sized to one large QueryFont reply. A client that
/// stopped reading for a while was dropped at that cap, where Xorg would
/// have kept its output.
pub const OUTBOUND_CAP: usize = i32::MAX as usize;

/// Queued output this large is logged, and each doubling past it: a
/// client that stopped reading, and what it is sent.
const BACKLOG_REPORT: usize = 1024 * 1024;

#[cfg(test)]
thread_local! {
    /// The cap tests saturate, a test thread's own: 4 MiB unless a test
    /// sets it, so a saturated client costs no more than that.
    static TEST_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(4 * 1024 * 1024) };
}

/// The cap in force: [`OUTBOUND_CAP`], or a test thread's.
pub fn outbound_cap() -> usize {
    #[cfg(test)]
    return TEST_CAP.with(std::cell::Cell::get);
    #[cfg(not(test))]
    OUTBOUND_CAP
}

/// Test support: run `f` with the production cap in force on this thread.
#[cfg(test)]
pub(crate) fn with_production_cap<T>(f: impl FnOnce() -> T) -> T {
    let before = TEST_CAP.with(|cap| cap.replace(OUTBOUND_CAP));
    let out = f();
    TEST_CAP.with(|cap| cap.set(before));
    out
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum WriteOutcome {
    /// Everything (queued + new) is on the wire.
    Done,
    /// Some bytes are buffered on `client.outbound`. Caller should
    /// register `WRITABLE` interest if not already.
    WouldBlock,
    /// Peer is gone or buffer exceeded `OUTBOUND_CAP`. The client is
    /// marked `write_failed`, so the core loop drops it.
    Disconnect,
}

/// Descriptors waiting in `ClientState::outbound`, each with the offset and
/// length there of its reply. A descriptor goes in the `SCM_RIGHTS` of a
/// `sendmsg` of its reply's bytes, as Xorg's `WriteFdToClient` (`os/io.c`)
/// leaves it to xtrans's next flush: queued with the output, never waited
/// for.
#[derive(Debug, Default)]
pub struct OutboundFds(VecDeque<QueuedFd>);

#[derive(Debug)]
struct QueuedFd {
    at: usize,
    len: usize,
    fd: OwnedFd,
}

impl OutboundFds {
    fn next_at(&self) -> Option<usize> {
        self.0.front().map(|queued| queued.at)
    }

    /// `n` bytes went, with the front descriptor when `with_front`.
    fn sent(&mut self, n: usize, with_front: bool) {
        if with_front {
            self.0.pop_front();
        }
        for queued in &mut self.0 {
            queued.at -= n;
        }
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// Queue `bytes` behind the client's output with `fd`, which their first
/// byte carries, and write what the socket takes now. The descriptor is
/// duplicated: the caller keeps and closes its own.
pub fn write_with_fd(
    client: &mut ClientState,
    bytes: &[u8],
    fd: RawFd,
) -> io::Result<WriteOutcome> {
    if client.gone {
        return Ok(WriteOutcome::Done);
    }
    if client.write_failed {
        return Ok(WriteOutcome::Disconnect);
    }
    let outcome = write_with_fd_inner(client, bytes, fd);
    note_failure(client, outcome)
}

fn write_with_fd_inner(
    client: &mut ClientState,
    bytes: &[u8],
    fd: RawFd,
) -> io::Result<WriteOutcome> {
    if !client.outbound.is_empty() && client.outbound.len() + bytes.len() > outbound_cap() {
        log::warn!(
            "client_io: client 0x{:x}: outbound cap exceeded -- {} bytes unread, +{} new with a descriptor; disconnecting",
            client.resource_id_base,
            client.outbound.len(),
            bytes.len(),
        );
        return Ok(WriteOutcome::Disconnect);
    }
    let fd = duplicate(fd)?;
    let at = client.outbound.len();
    client.outbound_fds.0.push_back(QueuedFd {
        at,
        len: bytes.len(),
        fd,
    });
    client.outbound.extend(bytes.iter().copied());
    drain_outbound_inner(client)
}

#[cfg(unix)]
fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: the caller's descriptor is open for the call.
    unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()
}

/// No Windows transport passes descriptors, so `fd_passing` is never set.
#[cfg(windows)]
fn duplicate(_fd: RawFd) -> io::Result<OwnedFd> {
    Err(unsupported())
}

fn unsupported() -> io::Error {
    io::Error::new(
        ErrorKind::Unsupported,
        "file descriptor passing is unavailable on this transport",
    )
}

/// One `sendmsg` of `chunk` with `fd`: the bytes it took.
#[cfg(unix)]
fn send_with_fd(writer: &mut Transport, chunk: &[u8], fd: &OwnedFd) -> io::Result<usize> {
    let Transport::Unix(stream) = writer else {
        return Err(unsupported());
    };
    match crate::unix_fd::send_with_fd(stream, chunk, std::os::fd::AsRawFd::as_raw_fd(fd)) {
        // The BSDs' unix stream socket (macOS's among them) shrinks the
        // sender's limit as the peer's buffer fills, and refuses a message
        // whose descriptor no longer fits with EMSGSIZE rather than EAGAIN.
        // It fits once the client reads.
        Err(e) if e.raw_os_error() == Some(libc::EMSGSIZE) => Err(ErrorKind::WouldBlock.into()),
        other => other,
    }
}

#[cfg(windows)]
fn send_with_fd(_writer: &mut Transport, _chunk: &[u8], fd: &OwnedFd) -> io::Result<usize> {
    match *fd {}
}

/// Write `bytes` to the client. Drains any already-buffered outbound
/// first so wire ordering is preserved.
pub fn write_or_buffer(client: &mut ClientState, bytes: &[u8]) -> io::Result<WriteOutcome> {
    // Xorg `WriteToClient` drops output to a gone client (`os/io.c:700`).
    if client.gone {
        return Ok(WriteOutcome::Done);
    }
    if client.write_failed {
        return Ok(WriteOutcome::Disconnect);
    }
    let outcome = write_or_buffer_inner(client, bytes);
    note_failure(client, outcome)
}

/// Clients whose writes failed and that the core loop must disconnect.
pub fn failed_writers(clients: &std::collections::HashMap<u32, ClientState>) -> Vec<ClientId> {
    let mut failed: Vec<ClientId> = clients
        .iter()
        .filter(|(_, client)| client.write_failed && !client.gone)
        .map(|(id, _)| ClientId(*id))
        .collect();
    failed.sort_unstable_by_key(|id| id.0);
    failed
}

fn note_failure(
    client: &mut ClientState,
    outcome: io::Result<WriteOutcome>,
) -> io::Result<WriteOutcome> {
    if !matches!(outcome, Ok(WriteOutcome::Done | WriteOutcome::WouldBlock)) {
        client.write_failed = true;
    }
    outcome
}

fn write_or_buffer_inner(client: &mut ClientState, bytes: &[u8]) -> io::Result<WriteOutcome> {
    // Drain pending bytes first.
    if !client.outbound.is_empty() {
        match drain_outbound_inner(client)? {
            WriteOutcome::Done => {} // fall through to write `bytes`
            WriteOutcome::WouldBlock => {
                return Ok(buffer_or_disconnect(client, bytes));
            }
            WriteOutcome::Disconnect => return Ok(WriteOutcome::Disconnect),
        }
    }

    // Outbound is empty; try a direct write.
    let writer_arc = client.writer.clone();
    let mut writer = writer_arc.lock().unwrap();
    match writer.write(bytes) {
        Ok(n) if n == bytes.len() => Ok(WriteOutcome::Done),
        Ok(0) => Ok(peer_gone(client, "write returned 0")),
        Ok(n) => {
            drop(writer);
            Ok(buffer_or_disconnect(client, &bytes[n..]))
        }
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            drop(writer);
            Ok(buffer_or_disconnect(client, bytes))
        }
        Err(e) if disconnect_kind(e.kind()) => Ok(peer_gone(client, &e.to_string())),
        Err(e) => Err(e),
    }
}

/// Push `bytes` onto `client.outbound`, or return `Disconnect` if
/// doing so would exceed the cap ([`OUTBOUND_CAP`]).
fn buffer_or_disconnect(client: &mut ClientState, bytes: &[u8]) -> WriteOutcome {
    let before = client.outbound.len();
    if before != 0 && before + bytes.len() > outbound_cap() {
        log::warn!(
            "client_io: client 0x{:x}: outbound cap exceeded -- {} bytes unread, +{} new; disconnecting",
            client.resource_id_base,
            before,
            bytes.len(),
        );
        return WriteOutcome::Disconnect;
    }
    client.outbound.extend(bytes.iter().copied());
    let after = client.outbound.len();
    if before == 0 {
        log::trace!("client_io: starting to buffer outbound -- {after} bytes (kernel buffer full)");
    }
    if crossed_report(before, after) {
        log::info!(
            "client_io: client 0x{:x} reads slowly: {} bytes unread; the newest message {}",
            client.resource_id_base,
            after,
            describe(bytes),
        );
    }
    WriteOutcome::WouldBlock
}

/// Whether the backlog went past [`BACKLOG_REPORT`] or one of its
/// doublings.
fn crossed_report(before: usize, after: usize) -> bool {
    let mut mark = BACKLOG_REPORT;
    while mark <= after {
        if before < mark {
            return true;
        }
        match mark.checked_mul(2) {
            Some(next) => mark = next,
            None => return false,
        }
    }
    false
}

/// An outgoing message by its header, for the backlog log: a reply, an
/// error, an event by code, a GenericEvent by extension and type.
fn describe(bytes: &[u8]) -> String {
    match bytes {
        [] => "(none)".into(),
        [0, code, ..] => format!("is error {code} ({} bytes)", bytes.len()),
        [1, ..] => format!("is a reply ({} bytes)", bytes.len()),
        [35, extension, _, _, _, _, _, _, low, high, ..] => format!(
            "is GenericEvent of extension {extension}, type {} ({} bytes)",
            u16::from_le_bytes([*low, *high]),
            bytes.len()
        ),
        [code, ..] => format!("is event {} ({} bytes)", code & 0x7f, bytes.len()),
    }
}

/// Push as much of `client.outbound` to the wire as possible without
/// blocking. Returns `Done` if the queue is now empty, `WouldBlock` if
/// some bytes remain, `Disconnect` if the peer is gone.
pub fn drain_outbound(client: &mut ClientState) -> io::Result<WriteOutcome> {
    if client.gone {
        client.outbound.clear();
        client.outbound_fds.clear();
        return Ok(WriteOutcome::Done);
    }
    if client.write_failed {
        return Ok(WriteOutcome::Disconnect);
    }
    let outcome = drain_outbound_inner(client);
    note_failure(client, outcome)
}

fn drain_outbound_inner(client: &mut ClientState) -> io::Result<WriteOutcome> {
    let writer_arc = client.writer.clone();
    let mut writer = writer_arc.lock().unwrap();
    while !client.outbound.is_empty() {
        // A descriptor goes with its reply's bytes alone, and any other
        // chunk ends where the next descriptor's reply begins.
        let with_fd = client.outbound_fds.next_at() == Some(0);
        let end = if with_fd {
            client.outbound_fds.0.front().map(|queued| queued.len)
        } else {
            client.outbound_fds.next_at()
        }
        .unwrap_or(usize::MAX);
        // VecDeque can wrap; iterate over the contiguous front slice
        // first. `as_slices().0` is the front; `.1` is everything past
        // the wrap point. Copy out a small chunk to dodge a borrow
        // conflict with the subsequent `drain` call.
        let chunk: Vec<u8> = {
            let (front, _) = client.outbound.as_slices();
            front[..front.len().min(8 * 1024).min(end)].to_vec()
        };
        let written = match client.outbound_fds.0.front() {
            Some(queued) if with_fd => send_with_fd(&mut writer, &chunk, &queued.fd),
            _ => writer.write(&chunk),
        };
        match written {
            Ok(0) => return Ok(peer_gone(client, "write returned 0")),
            Ok(n) => {
                client.outbound.drain(..n);
                client.outbound_fds.sent(n, with_fd);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(WriteOutcome::WouldBlock),
            Err(e) if disconnect_kind(e.kind()) => return Ok(peer_gone(client, &e.to_string())),
            Err(e) => return Err(e),
        }
    }
    Ok(WriteOutcome::Done)
}

/// The peer went: said once, since the client's writes stop here, so the
/// disconnect that follows says why.
fn peer_gone(client: &ClientState, why: &str) -> WriteOutcome {
    log::info!(
        "client_io: client 0x{:x}: the peer is gone ({why}), {} bytes unread",
        client.resource_id_base,
        client.outbound.len(),
    );
    WriteOutcome::Disconnect
}

fn disconnect_kind(k: ErrorKind) -> bool {
    matches!(
        k,
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
}

/// Test support: fill `client`'s socket, then its outbound buffer to
/// the cap ([`outbound_cap`]), so its next write overflows.
#[cfg(test)]
pub(crate) fn saturate_for_test(client: &mut ClientState) {
    client.writer.lock().unwrap().set_nonblocking(true).unwrap();
    client
        .outbound
        .extend(std::iter::repeat_n(0u8, outbound_cap()));
    let _ = drain_outbound(client);
    let missing = outbound_cap() - client.outbound.len();
    client.outbound.extend(std::iter::repeat_n(0u8, missing));
    assert!(!client.write_failed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::{ClientByteOrder, ResourceId};

    fn make_client(writer: UnixStream) -> ClientState {
        ClientState {
            admission: crate::access::Admission::Internal,
            writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: 0,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            outbound_fds: Default::default(),
            watching_writable: false,
            write_failed: false,
            gone: false,
            focused_window: ResourceId(0),
            reader_control: None,
            is_local: true,
            fd_passing: true,
            internal: false,
            peer: crate::peer::PeerIdentity::unknown_unix(),
        }
    }

    /// Small kernel send buffer makes it cheap to provoke EAGAIN.
    fn pair_with_small_sndbuf() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        // Shrink the write side so a few KiB exhausts the kernel buffer.
        unsafe {
            let bufsize: libc::c_int = 4096;
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&a),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::addr_of!(bufsize).cast(),
                std::mem::size_of_val(&bufsize) as libc::socklen_t,
            );
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&b),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                std::ptr::addr_of!(bufsize).cast(),
                std::mem::size_of_val(&bufsize) as libc::socklen_t,
            );
        }
        (a, b)
    }

    #[test]
    fn partial_write_returns_would_block_and_buffers_remainder() {
        let (a, mut b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        // Write enough that the kernel buffer fills mid-write. 32 KiB
        // is far over a 4 KiB SO_SNDBUF + receive window, so we get a
        // mix of accepted bytes and EAGAIN.
        let payload = vec![0xABu8; 32 * 1024];
        let outcome = write_or_buffer(&mut c, &payload).unwrap();
        // Either WouldBlock (some buffered) or Disconnect (overflowed cap).
        // For 32 KiB <= outbound_cap(), it should be WouldBlock.
        assert_eq!(outcome, WriteOutcome::WouldBlock);
        assert!(!c.outbound.is_empty(), "remainder should be buffered");
        // Drain the peer; outbound should now drain to empty.
        let mut sink = vec![0u8; 64 * 1024];
        let mut total = 0;
        b.set_nonblocking(true).unwrap();
        loop {
            match b.read(&mut sink[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        assert!(total > 0);
        let outcome = drain_outbound(&mut c).unwrap();
        // After draining the peer, our buffer should clear (possibly
        // requiring more peer reads in pathological cases — the small
        // payload makes one round enough).
        let _ = outcome;
    }

    #[test]
    fn buffer_overflow_returns_disconnect() {
        let (a, _b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        // Fill the kernel buffer first so the next call must buffer.
        let _ = write_or_buffer(&mut c, &vec![0u8; 8 * 1024]).unwrap();
        // Now hand it more than the cap — must Disconnect.
        let huge = vec![0u8; outbound_cap() + 1];
        let outcome = write_or_buffer(&mut c, &huge).unwrap();
        assert_eq!(outcome, WriteOutcome::Disconnect);
    }

    /// An overflow marks the client failed; nothing more reaches its socket
    /// or buffer, even after its peer catches up, and `failed_writers`
    /// names it for the core loop.
    #[test]
    fn overflow_marks_client_failed_and_refuses_later_writes() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut c = make_client(a);
        saturate_for_test(&mut c);
        assert_eq!(
            write_or_buffer(&mut c, &[1u8; 32]).unwrap(),
            WriteOutcome::Disconnect
        );
        assert!(c.write_failed);
        assert_eq!(c.outbound.len(), outbound_cap());

        b.set_nonblocking(true).unwrap();
        let mut sink = vec![0u8; 1 << 20];
        while b.read(&mut sink).is_ok_and(|n| n > 0) {}
        assert_eq!(
            write_or_buffer(&mut c, &[1u8; 32]).unwrap(),
            WriteOutcome::Disconnect
        );
        assert_eq!(drain_outbound(&mut c).unwrap(), WriteOutcome::Disconnect);
        assert_eq!(c.outbound.len(), outbound_cap());
        assert!(b.read(&mut sink).is_err(), "no byte after the failure");

        let mut clients = HashMap::new();
        clients.insert(9, c);
        let (ok, _ok_peer) = UnixStream::pair().unwrap();
        clients.insert(3, make_client(ok));
        assert_eq!(failed_writers(&clients), [ClientId(9)]);
    }

    /// A peer that hung up marks the client failed too.
    #[test]
    fn peer_gone_marks_client_failed() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        let mut c = make_client(a);
        assert_eq!(
            write_or_buffer(&mut c, &[1u8; 32]).unwrap(),
            WriteOutcome::Disconnect
        );
        assert!(c.write_failed);
    }

    /// A gone client's output is dropped (Xorg `WriteToClient`, `os/io.c:700`;
    /// `output_pending_clear`, `dix/dispatch.c:3522`): a write to its
    /// shut-down socket neither fails nor queues it for another disconnect.
    #[test]
    fn gone_client_drops_output_and_is_not_queued_for_disconnect() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        let mut c = make_client(a);
        c.outbound.extend([0u8; 32]);
        c.gone = true;
        assert_eq!(
            write_or_buffer(&mut c, &[1u8; 32]).unwrap(),
            WriteOutcome::Done
        );
        assert_eq!(drain_outbound(&mut c).unwrap(), WriteOutcome::Done);
        assert!(c.outbound.is_empty());
        assert!(!c.write_failed);
        c.write_failed = true;
        let mut clients = HashMap::new();
        clients.insert(9, c);
        assert!(failed_writers(&clients).is_empty());
    }

    /// A reply with a descriptor to a client that is not reading is queued
    /// at once, as Xorg's `WriteFdToClient` queues it: the core thread never
    /// waits for the client. Once the client reads, each descriptor arrives
    /// with its own reply's first byte, two replies in a row included.
    #[test]
    fn an_fd_reply_to_a_full_socket_is_queued_and_each_fd_goes_with_its_reply() {
        let (a, mut b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        let filler = vec![7u8; 64 * 1024];
        assert_eq!(
            write_or_buffer(&mut c, &filler).unwrap(),
            WriteOutcome::WouldBlock
        );
        let null = std::fs::File::open("/dev/null").unwrap();
        let null_fd = std::os::fd::AsRawFd::as_raw_fd(&null);
        let started = std::time::Instant::now();
        for reply in [[1u8; 32], [2u8; 32]] {
            assert_eq!(
                write_with_fd(&mut c, &reply, null_fd).unwrap(),
                WriteOutcome::WouldBlock
            );
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        drop(null);

        b.set_nonblocking(true).unwrap();
        let total = filler.len() + 64;
        let mut received = Vec::new();
        let mut fd_offsets = Vec::new();
        let mut buf = [0u8; 4096];
        while received.len() < total {
            let _ = drain_outbound(&mut c).unwrap();
            match crate::unix_fd::recv_with_fds(&mut b, &mut buf) {
                Ok((n, fds)) => {
                    for fd in fds {
                        fd_offsets.push(received.len());
                        // SAFETY: received for this test, closed once.
                        unsafe { libc::close(fd) };
                    }
                    received.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("recvmsg: {e}"),
            }
        }
        assert_eq!(fd_offsets, [filler.len(), filler.len() + 32]);
        assert_eq!(&received[filler.len()..filler.len() + 32], &[1u8; 32]);
        assert_eq!(&received[filler.len() + 32..], &[2u8; 32]);
        assert!(c.outbound.is_empty() && c.outbound_fds.0.is_empty());
    }

    #[test]
    fn single_oversized_reply_buffers_when_outbound_empty() {
        let (a, _b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        let big = vec![0u8; outbound_cap() + 1_000_000];
        let outcome = buffer_or_disconnect(&mut c, &big);
        assert_eq!(outcome, WriteOutcome::WouldBlock);
        assert_eq!(c.outbound.len(), big.len());
    }

    #[test]
    fn accumulation_past_cap_still_disconnects() {
        let (a, _b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        c.outbound
            .extend(std::iter::repeat_n(0u8, outbound_cap() - 10));
        let outcome = buffer_or_disconnect(&mut c, &[0u8; 1000]);
        assert_eq!(outcome, WriteOutcome::Disconnect);
    }

    #[test]
    fn drain_after_peer_reads_empties_buffer() {
        let (a, mut b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        // Force buffering by writing while peer hasn't read.
        let payload = vec![0x55u8; 16 * 1024];
        let _ = write_or_buffer(&mut c, &payload).unwrap();
        // Now peer reads everything.
        b.set_nonblocking(true).unwrap();
        let mut sink = vec![0u8; 64 * 1024];
        loop {
            match b.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        // Repeated drain should eventually empty `outbound`.
        for _ in 0..32 {
            if c.outbound.is_empty() {
                break;
            }
            let _ = drain_outbound(&mut c).unwrap();
            // Let kernel make progress.
            loop {
                match b.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
        assert!(c.outbound.is_empty(), "buffer should drain to empty");
    }

    /// A client that stopped reading was dropped once 4 MiB were queued
    /// for it. Xorg keeps buffering (`FlushClient`, up to `INT_MAX`): so does the core now.
    /// What that could cost instead: a writer stalled on the client that
    /// does not read, or bytes out of order. Neither: each write returns at
    /// once, a client that reads is served meanwhile, and the slow one gets
    /// every byte in order once it reads.
    #[test]
    fn a_client_that_stops_reading_keeps_its_output_as_xorg_does() {
        with_production_cap(|| {
            let (a, mut slow_peer) = pair_with_small_sndbuf();
            let mut slow = make_client(a);
            let (b, mut fast_peer) = UnixStream::pair().unwrap();
            b.set_nonblocking(true).unwrap();
            let mut fast = make_client(b);
            let event = |n: u32| {
                let mut bytes = [0u8; 32];
                bytes[0] = 6; // MotionNotify
                bytes[4..8].copy_from_slice(&n.to_le_bytes());
                bytes
            };
            // 16 MiB of events, four times the old cap, the peer reading
            // nothing (a write that waited would hang here); another
            // client's write between every 4096.
            let count = 16 * 1024 * 1024 / 32;
            for n in 0..count {
                let outcome = write_or_buffer(&mut slow, &event(n)).unwrap();
                assert_ne!(outcome, WriteOutcome::Disconnect, "event {n}");
                if n % 4096 == 0 {
                    assert_eq!(
                        write_or_buffer(&mut fast, &event(n)).unwrap(),
                        WriteOutcome::Done
                    );
                    let mut got = [0u8; 32];
                    fast_peer.read_exact(&mut got).unwrap();
                    assert_eq!(got, event(n));
                }
            }
            assert!(!slow.write_failed);
            assert!(slow.outbound.len() > 4 * 1024 * 1024);
            // The peer reads; every event arrives, in order.
            slow_peer.set_nonblocking(true).unwrap();
            let mut next = 0u32;
            let mut pending = Vec::new();
            let mut sink = vec![0u8; 64 * 1024];
            while next < count {
                let _ = drain_outbound(&mut slow).unwrap();
                match slow_peer.read(&mut sink) {
                    Ok(n) => pending.extend_from_slice(&sink[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => panic!("read: {e}"),
                }
                let whole = pending.len() / 32 * 32;
                for chunk in pending[..whole].chunks_exact(32) {
                    assert_eq!(chunk, event(next));
                    next += 1;
                }
                pending.drain(..whole);
            }
            assert!(slow.outbound.is_empty());
        });
    }

    #[test]
    fn a_backlog_is_reported_at_a_mebibyte_and_each_doubling() {
        let mib = 1024 * 1024;
        assert!(!crossed_report(0, mib - 1));
        assert!(crossed_report(mib - 1, mib));
        assert!(!crossed_report(mib, mib + 32));
        assert!(crossed_report(3 * mib, 4 * mib + 1));
        assert!(!crossed_report(4 * mib + 1, 5 * mib));
        assert!(!crossed_report(usize::MAX - 1, usize::MAX));
        let mut generic = [0u8; 32];
        generic[0] = 35;
        generic[1] = 131;
        generic[8] = 17;
        assert_eq!(
            describe(&generic),
            "is GenericEvent of extension 131, type 17 (32 bytes)"
        );
        assert_eq!(describe(&[0x86, 0, 0, 0]), "is event 6 (4 bytes)");
    }

    #[test]
    fn disconnect_with_pending_outbound_no_panic() {
        // The caller drops a ClientState whose outbound still has
        // bytes in it (e.g. backpressure-induced disconnect path).
        // No code path on the helper side observes this; we only
        // check that Drop doesn't panic.
        let (a, _b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        c.outbound.extend([1u8, 2, 3, 4]);
        drop(c); // would have panicked if any invariant assumed empty
    }
}
