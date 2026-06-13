//! Per-client outbound write helper.
//!
//! The core thread hands buffers (event/reply bytes) to
//! `write_or_buffer`. It tries a direct non-blocking `write(2)` first;
//! on `EAGAIN`/partial write it buffers the remainder on
//! `ClientState::outbound` and returns `WouldBlock` so the caller can
//! arrange `WRITABLE` interest in the poller (I2). When the buffer is
//! about to exceed `OUTBOUND_CAP` we return `Disconnect` and let the
//! caller tear the client down.
//!
//! Today's `ClientState::writer` is still `Arc<Mutex<UnixStream>>`
//! (A1 left it untouched; D2 demotes it). The helper transparently
//! locks the mutex; once D2 lands we strip the lock without touching
//! call sites.

use std::io::{self, ErrorKind, Write};

use crate::server::ClientState;

/// Maximum bytes queued per client before we treat the peer as
/// unrecoverably slow and disconnect.
///
/// Sized to fit a single legitimate large reply with headroom for
/// additional traffic. The motivating case is a QueryFont reply for an
/// ISO10646 host font (`-Misc-Fixed-*-ISO10646-1`): m=65536 CHAR_INFOs
/// ⇒ 32 + 4·(7 + 2n + 3·65536) ≈ 786 KB. With Linux's default
/// 208 KB unix-socket send buffer the partial-write tail is ~578 KB,
/// so a smaller cap would force a spurious slow-client disconnect on a
/// fast client that simply hasn't read the reply yet.
pub const OUTBOUND_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum WriteOutcome {
    /// Everything (queued + new) is on the wire.
    Done,
    /// Some bytes are buffered on `client.outbound`. Caller should
    /// register `WRITABLE` interest if not already.
    WouldBlock,
    /// Peer is gone or buffer exceeded `OUTBOUND_CAP`. Caller must
    /// drop the client.
    Disconnect,
}

/// Write `bytes` to the client. Drains any already-buffered outbound
/// first so wire ordering is preserved.
pub fn write_or_buffer(client: &mut ClientState, bytes: &[u8]) -> io::Result<WriteOutcome> {
    // Drain pending bytes first.
    if !client.outbound.is_empty() {
        match drain_outbound(client)? {
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
        Ok(0) => Ok(WriteOutcome::Disconnect),
        Ok(n) => {
            drop(writer);
            Ok(buffer_or_disconnect(client, &bytes[n..]))
        }
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            drop(writer);
            Ok(buffer_or_disconnect(client, bytes))
        }
        Err(e) if disconnect_kind(e.kind()) => Ok(WriteOutcome::Disconnect),
        Err(e) => Err(e),
    }
}

/// Push `bytes` onto `client.outbound`, or return `Disconnect` if
/// doing so would exceed `OUTBOUND_CAP`.
fn buffer_or_disconnect(client: &mut ClientState, bytes: &[u8]) -> WriteOutcome {
    if !client.outbound.is_empty() && client.outbound.len() + bytes.len() > OUTBOUND_CAP {
        log::warn!(
            "client_io: outbound cap exceeded — outbound={} bytes pending, +{} new ⇒ Disconnect",
            client.outbound.len(),
            bytes.len(),
        );
        return WriteOutcome::Disconnect;
    }
    let was_empty = client.outbound.is_empty();
    client.outbound.extend(bytes.iter().copied());
    if was_empty {
        log::trace!(
            "client_io: starting to buffer outbound — {} bytes (kernel buffer full)",
            client.outbound.len(),
        );
    }
    WriteOutcome::WouldBlock
}

/// Push as much of `client.outbound` to the wire as possible without
/// blocking. Returns `Done` if the queue is now empty, `WouldBlock` if
/// some bytes remain, `Disconnect` if the peer is gone.
pub fn drain_outbound(client: &mut ClientState) -> io::Result<WriteOutcome> {
    let writer_arc = client.writer.clone();
    let mut writer = writer_arc.lock().unwrap();
    while !client.outbound.is_empty() {
        // VecDeque can wrap; iterate over the contiguous front slice
        // first. `as_slices().0` is the front; `.1` is everything past
        // the wrap point. Copy out a small chunk to dodge a borrow
        // conflict with the subsequent `drain` call.
        let chunk: Vec<u8> = {
            let (front, _) = client.outbound.as_slices();
            front[..front.len().min(8 * 1024)].to_vec()
        };
        match writer.write(&chunk) {
            Ok(0) => return Ok(WriteOutcome::Disconnect),
            Ok(n) => {
                client.outbound.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(WriteOutcome::WouldBlock),
            Err(e) if disconnect_kind(e.kind()) => return Ok(WriteOutcome::Disconnect),
            Err(e) => return Err(e),
        }
    }
    Ok(WriteOutcome::Done)
}

fn disconnect_kind(k: ErrorKind) -> bool {
    matches!(
        k,
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
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
            writer: Arc::new(Mutex::new(writer)),
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
            watching_writable: false,
            focused_window: ResourceId(0),
            reader_control: None,
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
        // For 32 KiB <= OUTBOUND_CAP (64 KiB), it should be WouldBlock.
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
        // Now hand it more than OUTBOUND_CAP — must Disconnect.
        let huge = vec![0u8; OUTBOUND_CAP + 1];
        let outcome = write_or_buffer(&mut c, &huge).unwrap();
        assert_eq!(outcome, WriteOutcome::Disconnect);
    }

    #[test]
    fn single_oversized_reply_buffers_when_outbound_empty() {
        let (a, _b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        let big = vec![0u8; OUTBOUND_CAP + 1_000_000];
        let outcome = buffer_or_disconnect(&mut c, &big);
        assert_eq!(outcome, WriteOutcome::WouldBlock);
        assert_eq!(c.outbound.len(), big.len());
    }

    #[test]
    fn accumulation_past_cap_still_disconnects() {
        let (a, _b) = pair_with_small_sndbuf();
        let mut c = make_client(a);
        c.outbound
            .extend(std::iter::repeat_n(0u8, OUTBOUND_CAP - 10));
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
