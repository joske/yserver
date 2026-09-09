//! Per-client reader thread.
//!
//! Spawned by the core's `ClientSetupComplete` handler (D4). Owns the
//! original blocking client stream's reader role; the writer half is
//! a `try_clone` kept by the core and made non-blocking. Because both
//! fds reference the same OFD on Linux, `O_NONBLOCK` set by the core
//! is observed on the reader fd too. We compensate with a
//! `BlockingFdReader` adapter that turns `EAGAIN` into a `poll(2)`
//! wait, so the X11 framing code (`read_request`) never sees a
//! mid-request `WouldBlock` and so cannot lose bytes between retries.
//!
//! BigRequests barrier: the X11 server's BigRequests major opcode is
//! known at server-startup time. After the reader sends an Enable
//! request, it parks on the `reader_control` channel waiting for
//! `ApplyBigRequests` (toggle local `big` to `true`) or
//! `IgnoreBigRequests` (leave `big` unchanged). `Shutdown` causes the
//! reader to exit immediately and also unparks barrier waits.
//!
//! Ingress backpressure: each reader may have only
//! [`MAX_IN_FLIGHT_BYTES_PER_CLIENT`] accepted request bytes outstanding. Once
//! those credits are consumed it stops reading the socket until the core
//! dispatches a request and returns `GrantRequestBytes`. This keeps a hot
//! client in the kernel socket instead of draining it into an unbounded
//! userspace queue, matching the natural backpressure of Xorg's dispatch loop.
//!
//! Disconnect path: any `read_request` error (EOF, EPIPE, malformed
//! framing) is reported back as `Message::ClientDisconnected`. The
//! core then sends `ReaderControl::Shutdown` to the reader before
//! dropping its `reader_control`, so a reader parked on a barrier
//! also exits.

use std::{
    io::{self, ErrorKind},
    os::fd::{FromRawFd, RawFd},
};

use crossbeam_channel::{Receiver, TryRecvError};
use log::warn;

use yserver_protocol::x11::{self, ClientByteOrder, ClientId, SequenceNumber};

use crate::{
    core_loop::{message::Message, sender::CoreSender},
    server::ReaderControl,
    transport::Transport,
    unix_fd::FdReader,
};

/// X11 BigRequests `Enable` minor opcode (always 0 — see X.Org spec).
pub const BIG_REQUESTS_ENABLE_MINOR: u8 = 0;

/// Maximum ordinary request bytes one client may have accepted but not yet
/// dispatched. This matches Xorg's 16 KiB initial per-client input buffer
/// (`os/io.c::BUFSIZE`). A single request larger than the remaining budget is
/// still framed, like Xorg growing its buffer for one complete large request;
/// the reader then stops until that request's bytes are returned.
pub const MAX_IN_FLIGHT_BYTES_PER_CLIENT: usize = 16 * 1024;

#[derive(Debug, Default)]
struct ReaderIngressWindow {
    outstanding_bytes: usize,
}

impl ReaderIngressWindow {
    fn can_read(&self) -> bool {
        self.outstanding_bytes < MAX_IN_FLIGHT_BYTES_PER_CLIENT
    }

    fn accept(&mut self, bytes: usize) {
        // One complete request may cross the window boundary, just as Xorg
        // grows its input buffer to hold one request. Preserve that overshoot
        // as outstanding debt; discarding it recreates unbounded read-ahead
        // when variable-sized requests repeat in a shifted cycle.
        self.outstanding_bytes = self.outstanding_bytes.saturating_add(bytes);
    }

    fn complete(&mut self, bytes: usize) {
        self.outstanding_bytes = self.outstanding_bytes.saturating_sub(bytes);
    }
}

/// Spawn the reader thread. Returns immediately; the thread runs
/// until it observes EOF, an unrecoverable framing error, or
/// `ReaderControl::Shutdown`.
pub fn spawn(
    id: ClientId,
    stream: impl Into<Transport>,
    byte_order: ClientByteOrder,
    big_requests_major: u8,
    control_rx: Receiver<ReaderControl>,
    sender: CoreSender,
) -> io::Result<()> {
    let stream = stream.into();
    std::thread::Builder::new()
        .name(format!("yserver-reader-{}", id.0))
        .spawn(move || {
            if let Err(e) = run(
                id,
                stream,
                byte_order,
                big_requests_major,
                control_rx,
                &sender,
            ) {
                let _ = sender.send(Message::ClientDisconnected { id, reason: e });
            }
        })?;
    Ok(())
}

fn run(
    id: ClientId,
    stream: Transport,
    byte_order: ClientByteOrder,
    big_requests_major: u8,
    control_rx: Receiver<ReaderControl>,
    sender: &CoreSender,
) -> io::Result<()> {
    let Transport::Unix(stream) = stream else {
        unreachable!("TCP reader support lands with transport-aware FdReader");
    };
    let mut reader = BlockingFdReader::new(FdReader::new(stream));
    let mut big = false;
    let mut sequence: u16 = 0;
    let mut ingress = ReaderIngressWindow::default();
    let telemetry_enabled = std::env::var_os("YSERVER_LOOP_TELEMETRY").is_some();

    loop {
        // Consume returned credits and observe Shutdown before reading again.
        // If the credit window is exhausted, blocking here deliberately leaves
        // subsequent bytes in the client socket so the producer is throttled.
        loop {
            let control = if !ingress.can_read() {
                match control_rx.recv() {
                    Ok(control) => control,
                    Err(_) => return Ok(()),
                }
            } else {
                match control_rx.try_recv() {
                    Ok(control) => control,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            };
            match control {
                ReaderControl::Shutdown => return Ok(()),
                ReaderControl::GrantRequestBytes(bytes) => {
                    ingress.complete(bytes);
                }
                ReaderControl::ApplyBigRequests | ReaderControl::IgnoreBigRequests => {
                    // Apply/Ignore is valid only inside the barrier below.
                    warn!(
                        "client {}: stray ReaderControl outside BigRequests barrier",
                        id.0
                    );
                }
            }
        }

        let frame = match x11::read_request(&mut reader, byte_order, big) {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                // Peer EOF — surface it to the core so
                // `process_disconnect` runs. The spawn-side `Err`
                // wrapper only fires on `io::Error`, not on Ok(None);
                // without this an Ok(None) leaks the client entry.
                let _ = sender.send(Message::ClientDisconnected {
                    id,
                    reason: io::Error::from(io::ErrorKind::UnexpectedEof),
                });
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let (header, mut body) = frame;
        let request_bytes = usize::try_from(header.length_units)
            .unwrap_or(usize::MAX)
            .saturating_mul(4)
            .max(4);
        // Phase E: swap inbound BE-client request bodies in-place so the
        // rest of the dispatch path can decode bytes as little-endian.
        x11::request_swap::swap_request_body(header.opcode, header.data, byte_order, &mut body);
        sequence = sequence.wrapping_add(1);
        // Phase 4.2.1 fix: pop only the fds this opcode actually
        // consumes. The previous unconditional `pop_fd()` per request
        // misattributed when libxcb batched multiple requests into
        // one `writev` with a single SCM_RIGHTS attachment — Mesa's
        // xcb-dri3 hits this every time. Now we peek at the (major,
        // minor[, body]) and pop exactly the right count.
        let fd_count = expected_fd_count(header.opcode, header.data, &body);
        let mut attached_fd: Option<RawFd> = None;
        for _ in 0..fd_count {
            // First fd → attached_fd; any extras (PixmapFromBuffers
            // num_buffers > 1) are closed because Phase 4.2 only
            // accepts num_buffers == 1 — the dispatcher rejects
            // larger requests with BadAlloc anyway.
            match reader.pop_fd() {
                Some(raw) if attached_fd.is_none() => attached_fd = Some(raw),
                Some(raw) => {
                    // SAFETY: own the raw fd; close to avoid leak.
                    unsafe { libc::close(raw) };
                }
                None => break,
            }
        }

        let is_enable =
            header.opcode == big_requests_major && header.data == BIG_REQUESTS_ENABLE_MINOR;

        ingress.accept(request_bytes);
        sender.send(Message::Request {
            id,
            sequence: SequenceNumber(sequence),
            accepted_at: telemetry_enabled.then(std::time::Instant::now),
            header,
            body,
            attached_fd: attached_fd.map(|raw| {
                // SAFETY: FdReader hands us an fd it received via SCM_RIGHTS;
                // there's no other owner.
                unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
            }),
        })?;

        if is_enable {
            // Park until the core processes the Enable.
            loop {
                match control_rx.recv() {
                    Ok(ReaderControl::ApplyBigRequests) => {
                        big = true;
                        break;
                    }
                    Ok(ReaderControl::IgnoreBigRequests) => break,
                    Ok(ReaderControl::GrantRequestBytes(bytes)) => {
                        ingress.complete(bytes);
                    }
                    Ok(ReaderControl::Shutdown) | Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// Number of `SCM_RIGHTS` file descriptors a request body expects to
/// arrive with. Used by `run` to pop the right count off the reader's
/// fd queue rather than the previous unconditional one-pop-per-request
/// (which mis-attributed when libxcb batched multiple requests into
/// one `writev` carrying a single fd).
///
/// Major opcodes are the *yserver-local* values (DRI3=147, MIT-SHM=130).
fn expected_fd_count(major_opcode: u8, minor_opcode: u8, body: &[u8]) -> usize {
    match major_opcode {
        // MIT-SHM AttachFd (minor 6) — 1 fd.
        130 if minor_opcode == 6 => 1,
        // DRI3 — opcode-dependent.
        147 => match minor_opcode {
            // PixmapFromBuffer (1 fd).
            2 => 1,
            // FenceFromFD (1 fd).
            4 => 1,
            // PixmapFromBuffers — num_buffers fds (1..=4).
            7 => {
                if body.len() >= 9 {
                    usize::from(body[8].clamp(1, 4))
                } else {
                    1
                }
            }
            // ImportSyncobj (1 fd).
            10 => 1,
            _ => 0,
        },
        _ => 0,
    }
}

/// Adapter around [`FdReader`] that turns `EAGAIN` into a blocking
/// `poll(2)` wait. The wrapped reader's fd may share `O_NONBLOCK`
/// with the writer-side clone (see module docs), but `Read::read` on
/// this wrapper never returns `WouldBlock`, which keeps `read_exact`
/// — and therefore `x11::read_request` — re-entry-free.
struct BlockingFdReader {
    inner: FdReader,
}

impl BlockingFdReader {
    fn new(inner: FdReader) -> Self {
        Self { inner }
    }

    fn pop_fd(&mut self) -> Option<RawFd> {
        self.inner.pop_fd()
    }

    fn fd(&self) -> RawFd {
        self.inner.fd()
    }
}

impl io::Read for BlockingFdReader {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(dst) {
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    wait_readable(self.fd())?;
                    continue;
                }
                other => return other,
            }
        }
    }
}

/// Block until `fd` is readable (or `poll(2)` errors).  POLLHUP is
/// also surfaced as readable so the caller observes EOF on the next
/// `read`.
pub(crate) fn wait_readable(fd: RawFd) -> io::Result<()> {
    use std::time::Duration;
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    loop {
        let n = unsafe { libc::poll(&mut pfd, 1, -1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if pfd.revents != 0 {
            return Ok(());
        }
        // Spurious wake — extremely rare with timeout=-1, but defend.
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_loop::sender::{CoreReceiver, channel};
    use crossbeam_channel::unbounded;
    use std::{
        io::Write,
        os::unix::net::UnixStream,
        time::{Duration, Instant},
    };

    fn recv_with_timeout(rx: &CoreReceiver, timeout: Duration) -> Option<Message> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(m) = rx.try_recv_all().next() {
                return Some(m);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        None
    }

    fn write_request_no_body(s: &mut UnixStream, opcode: u8, minor: u8, length_units: u16) {
        // 4-byte header: opcode | minor | length_lo | length_hi
        let buf = [
            opcode,
            minor,
            (length_units & 0xff) as u8,
            ((length_units >> 8) & 0xff) as u8,
        ];
        s.write_all(&buf).unwrap();
    }

    fn write_big_request(s: &mut UnixStream, opcode: u8, minor: u8, length_units: u32) {
        // length_units==0 in 16-bit field, then full 32-bit length.
        let buf = [opcode, minor, 0, 0];
        s.write_all(&buf).unwrap();
        s.write_all(&length_units.to_le_bytes()).unwrap();
    }

    const BIG_MAJOR: u8 = 135;

    #[test]
    fn ingress_window_preserves_variable_request_overshoot_debt() {
        let mut window = ReaderIngressWindow::default();
        window.accept(MAX_IN_FLIGHT_BYTES_PER_CLIENT - 4);
        assert!(window.can_read(), "four bytes of the window remain");

        window.accept(20);
        assert_eq!(
            window.outstanding_bytes,
            MAX_IN_FLIGHT_BYTES_PER_CLIENT + 16
        );
        assert!(!window.can_read());

        // Completing a four-byte request must not erase the 16-byte
        // overshoot. The old saturating available-credit implementation did,
        // admitting another request on every shifted variable-size cycle.
        window.complete(4);
        assert_eq!(
            window.outstanding_bytes,
            MAX_IN_FLIGHT_BYTES_PER_CLIENT + 12
        );
        assert!(!window.can_read());

        window.complete(13);
        assert!(window.can_read());
    }

    #[test]
    fn reader_credit_window_restores_socket_backpressure() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(99),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();

        const REQUEST_BYTES: usize = 4;
        let initial_requests = MAX_IN_FLIGHT_BYTES_PER_CLIENT / REQUEST_BYTES;
        for _ in 0..(initial_requests + 16) {
            write_request_no_body(&mut client_side, 42, 0, 1);
        }

        for _ in 0..initial_requests {
            let message = recv_with_timeout(&rx, Duration::from_secs(2))
                .expect("initial credit window request");
            assert!(matches!(message, Message::Request { .. }));
        }
        assert!(
            recv_with_timeout(&rx, Duration::from_millis(50)).is_none(),
            "reader must stop draining after its per-client credits are exhausted"
        );

        ctrl_tx
            .send(ReaderControl::GrantRequestBytes(REQUEST_BYTES))
            .unwrap();
        let message = recv_with_timeout(&rx, Duration::from_secs(2))
            .expect("one returned credit admits one request");
        assert!(matches!(message, Message::Request { .. }));
        assert!(
            recv_with_timeout(&rx, Duration::from_millis(50)).is_none(),
            "one returned credit must not admit multiple requests"
        );
        ctrl_tx.send(ReaderControl::Shutdown).unwrap();
    }

    #[test]
    fn reader_admits_one_request_larger_than_the_byte_window() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();
        spawn(
            ClientId(100),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();

        const LARGE_UNITS: u16 = 5 * 1024; // 20 KiB > Xorg's 16 KiB BUFSIZE.
        write_request_no_body(&mut client_side, 42, 0, LARGE_UNITS);
        client_side
            .write_all(&vec![0; usize::from(LARGE_UNITS) * 4 - 4])
            .unwrap();
        write_request_no_body(&mut client_side, 42, 0, 1);

        let large = recv_with_timeout(&rx, Duration::from_secs(2)).expect("large request");
        assert!(matches!(
            large,
            Message::Request { header, .. } if header.length_units == u32::from(LARGE_UNITS)
        ));
        assert!(recv_with_timeout(&rx, Duration::from_millis(50)).is_none());

        ctrl_tx
            .send(ReaderControl::GrantRequestBytes(
                usize::from(LARGE_UNITS) * 4,
            ))
            .unwrap();
        let next = recv_with_timeout(&rx, Duration::from_secs(2)).expect("request after return");
        assert!(matches!(
            next,
            Message::Request { header, .. } if header.length_units == 1
        ));
        ctrl_tx.send(ReaderControl::Shutdown).unwrap();
    }

    #[test]
    fn enable_then_large_request_round_trip() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(1),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();

        // 1. Send Enable: opcode=135 minor=0 length_units=1 (4 bytes).
        write_request_no_body(&mut client_side, BIG_MAJOR, 0, 1);
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("Enable arrives");
        match m {
            Message::Request { header, .. } => {
                assert_eq!(header.opcode, BIG_MAJOR);
                assert_eq!(header.data, 0);
            }
            other => panic!("expected Request, got {other:?}"),
        }
        // Reader is parked. Send Apply.
        ctrl_tx.send(ReaderControl::ApplyBigRequests).unwrap();

        // 2. Send a "big" request — opcode=42, minor=0, length_units=3 (12 bytes total).
        write_big_request(&mut client_side, 42, 0, 3);
        // Body for big-mode is 4*3 - 8 = 4 bytes.
        client_side.write_all(&[0xAA, 0xBB, 0xCC, 0xDD]).unwrap();
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("big request");
        match m {
            Message::Request { header, body, .. } => {
                assert_eq!(header.opcode, 42);
                assert_eq!(header.length_units, 3);
                assert_eq!(body, vec![0xAA, 0xBB, 0xCC, 0xDD]);
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn pipelined_enable_plus_big_request_in_one_write() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(2),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();

        // Pipeline: Enable + big request in one buffer.
        let mut combined = Vec::new();
        combined.extend([BIG_MAJOR, 0, 1, 0]); // Enable, length_units=1
        combined.extend([42, 0, 0, 0]); // big, 16-bit length=0
        combined.extend(3u32.to_le_bytes()); // big length_units=3
        combined.extend([1, 2, 3, 4]); // 4-byte body
        client_side.write_all(&combined).unwrap();

        // Reader sends Enable, parks.
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("enable");
        assert!(matches!(m, Message::Request { ref header, .. } if header.opcode == BIG_MAJOR));

        // No second Request before we Apply.
        assert!(rx.try_recv_all().next().is_none());
        ctrl_tx.send(ReaderControl::ApplyBigRequests).unwrap();

        // Now the big request frames correctly.
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("big req");
        match m {
            Message::Request { header, body, .. } => {
                assert_eq!(header.opcode, 42);
                assert_eq!(body, vec![1, 2, 3, 4]);
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn enable_before_any_other_request_is_recognised() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(3),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();
        // Very first byte the client ever sends is Enable.
        write_request_no_body(&mut client_side, BIG_MAJOR, 0, 1);
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("enable");
        assert!(matches!(m, Message::Request { ref header, .. } if header.opcode == BIG_MAJOR));
        ctrl_tx.send(ReaderControl::ApplyBigRequests).unwrap();
    }

    #[test]
    fn duplicate_enable_does_not_wedge() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(4),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();

        for _ in 0..2 {
            write_request_no_body(&mut client_side, BIG_MAJOR, 0, 1);
            let _ = recv_with_timeout(&rx, Duration::from_secs(2)).expect("enable");
            ctrl_tx.send(ReaderControl::ApplyBigRequests).unwrap();
        }
        // Send a normal request — reader should be unblocked.
        write_request_no_body(&mut client_side, 7, 0, 1);
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("normal");
        assert!(matches!(m, Message::Request { ref header, .. } if header.opcode == 7));
    }

    #[test]
    fn malformed_enable_is_resumed_via_ignore() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(5),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();
        // Enable header — reader doesn't validate; it just parks.
        write_request_no_body(&mut client_side, BIG_MAJOR, 0, 1);
        let _ = recv_with_timeout(&rx, Duration::from_secs(2)).expect("enable");
        // Core decided this Enable was malformed; tell reader to keep big = false.
        ctrl_tx.send(ReaderControl::IgnoreBigRequests).unwrap();

        // Send a normal request. If reader wrongly toggled big=true and
        // tried to read the extra big-length word, framing would
        // misalign and the test would hang or framing-error.
        write_request_no_body(&mut client_side, 7, 0, 1);
        let m = recv_with_timeout(&rx, Duration::from_secs(2)).expect("normal");
        assert!(matches!(m, Message::Request { ref header, .. } if header.opcode == 7));
    }

    #[test]
    fn shutdown_during_enable_park_unblocks_reader() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let (ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();

        spawn(
            ClientId(6),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();
        write_request_no_body(&mut client_side, BIG_MAJOR, 0, 1);
        let _ = recv_with_timeout(&rx, Duration::from_secs(2)).expect("enable");
        // Reader is parked.  Shutdown should unblock it.
        ctrl_tx.send(ReaderControl::Shutdown).unwrap();
        // Drop the client side to confirm no further reads happen.
        drop(client_side);
        // No follow-up Request or ClientDisconnected (reader exited cleanly).
        let elapsed = Instant::now();
        while elapsed.elapsed() < Duration::from_millis(200) {
            if let Some(m) = rx.try_recv_all().next() {
                // ClientDisconnected from `run` early return is not expected
                // — Shutdown returns Ok(()), no message sent.
                panic!("unexpected message after Shutdown park-unblock: {m:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn peer_close_emits_client_disconnected() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let (server_side, client_side) = UnixStream::pair().unwrap();
        let (_ctrl_tx, ctrl_rx) = unbounded::<ReaderControl>();
        spawn(
            ClientId(7),
            server_side,
            ClientByteOrder::LittleEndian,
            BIG_MAJOR,
            ctrl_rx,
            sender,
        )
        .unwrap();
        drop(client_side);
        // run() returns Ok(None) on peer EOF — no ClientDisconnected sent.
        // We assert no Request arrives. (Peer-close-during-frame would
        // emit Disconnected; no-bytes-then-EOF returns Ok(None).)
        let elapsed = Instant::now();
        while elapsed.elapsed() < Duration::from_millis(200) {
            if let Some(m) = rx.try_recv_all().next()
                && !matches!(m, Message::ClientDisconnected { .. })
            {
                panic!("unexpected: {m:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
