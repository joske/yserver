//! Per-client setup thread.
//!
//! On every accepted connection the core spawns one of these. It does
//! the *entire* X11 handshake on its own thread:
//!
//! 1. `read_setup_request` (blocking with `SO_RCVTIMEO`).
//! 2. Validate byte order.
//! 3. Send `Message::SetupAllocate` to the core; block on the
//!    rendezvous reply with allocated resource ids + screen geometry.
//! 4. `write_setup_success` (blocking with `SO_SNDTIMEO`).
//! 5. Send `Message::ClientSetupComplete` handing the still-blocking
//!    stream back to the core for split + register + reader spawn (D4).
//!
//! Teardown registry: a clone of every accepted stream lives in a
//! shared `SetupRegistry`. On `Message::Shutdown` the core iterates the
//! registry and calls `shutdown(Shutdown::Both)` on each clone, which
//! unblocks any in-flight `read`/`write` on the corresponding setup
//! thread (Linux Unix-socket OFD sharing). The setup thread propagates
//! the `EOF`/`EPIPE` as an `io::Error`, drops its stream, and exits.
//! A `SetupGuard` `Drop` impl ensures the registry entry is removed on
//! every exit path (success, error, panic).

use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam_channel::bounded;
use log::{debug, warn};

use yserver_protocol::x11::{self, ClientId};

use crate::{
    core_loop::{
        auth::{AuthState, AuthTransport, AuthVerdict},
        message::{Message, SetupAllocateResponse},
        sender::BoundSender,
    },
    resources::{ARGB_VISUAL, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW},
    transport::Transport,
};

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared registry of setup-stage streams. The setup thread holds
/// the *original* stream; this map holds a `try_clone` so the core can
/// `shutdown(Both)` it on shutdown to unblock the setup thread.
pub type SetupRegistry = Arc<Mutex<HashMap<ClientId, Transport>>>;

pub fn make_registry() -> SetupRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Spawn a setup thread. The clone is inserted into `registry`
/// synchronously, before the thread starts, so a shutdown that races
/// the spawn cannot miss it. Locality and fd capability come from the
/// accepting listener and survive the setup handoff to `ClientState`.
///
/// `sender` must be bound to the generation running at **accept** (see
/// [`CoreSender::bind`]). A setup thread outlives the session boundary
/// in the worst case — `reset_generation` shuts its socket down, but it
/// may already hold a decoded `ClientSetupComplete` — so its tag has to
/// name the session the connection belongs to, not the one running when
/// it happens to wake.
///
/// [`CoreSender::bind`]: crate::core_loop::sender::CoreSender::bind
pub fn spawn(
    id: ClientId,
    stream: impl Into<Transport>,
    sender: BoundSender,
    registry: SetupRegistry,
    auth: Arc<AuthState>,
    is_local: bool,
    fd_passing: bool,
) -> io::Result<()> {
    let stream = stream.into();
    let cloned = stream.try_clone()?;
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, cloned);

    let registry_for_thread = registry.clone();
    std::thread::Builder::new()
        .name(format!("yserver-setup-{}", id.0))
        .spawn(move || {
            let _guard = SetupGuard {
                id,
                registry: registry_for_thread,
            };
            if let Err(e) = run_setup(id, stream, &sender, &auth, is_local, fd_passing) {
                // ConnectionAborted/UnexpectedEof on shutdown is
                // expected; anything else is worth a warn.
                if !matches!(
                    e.kind(),
                    ErrorKind::UnexpectedEof
                        | ErrorKind::BrokenPipe
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                        | ErrorKind::WouldBlock
                        | ErrorKind::TimedOut
                ) {
                    warn!("setup thread {}: {e}", id.0);
                }
            }
        })?;
    Ok(())
}

/// Walk the registry and shutdown every entry. Drops each clone after
/// shutdown so the kernel releases the fd. Setup threads' blocked
/// syscalls return errors on the next iteration.
pub fn shutdown_all(registry: &SetupRegistry) {
    let mut map = match registry.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    for (_id, s) in map.drain() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

struct SetupGuard {
    id: ClientId,
    registry: SetupRegistry,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.registry.lock() {
            g.remove(&self.id);
        }
    }
}

fn run_setup(
    id: ClientId,
    mut stream: Transport,
    sender: &BoundSender,
    auth: &AuthState,
    is_local: bool,
    fd_passing: bool,
) -> io::Result<()> {
    stream.set_read_timeout(Some(SETUP_TIMEOUT))?;
    stream.set_write_timeout(Some(SETUP_TIMEOUT))?;

    let setup = x11::read_setup_request(&mut stream)?;
    debug!(
        "client {} setup: byte_order={:?} protocol {}.{}",
        id.0, setup.byte_order, setup.protocol_major, setup.protocol_minor
    );

    // Authorization follows the actual socket, independently of extension
    // capability metadata: TCP must never take the local-open fallback.
    let auth_transport = match &stream {
        Transport::Unix(_) => AuthTransport::Unix,
        Transport::Tcp(_) => AuthTransport::Tcp,
    };
    // The generation handed to `check` is THIS thread's producer binding,
    // captured at accept — never the counter's current value. An XDMCP
    // session credential is bound to the generation its `Accept` belonged to,
    // so a connection accepted in a destroyed session fails the comparison
    // even if it reaches this line long after the reset, and even though the
    // new session has by then installed a credential of its own.
    if let AuthVerdict::Reject(reason) = auth.check(
        auth_transport,
        sender.generation(),
        &setup.auth_protocol_name,
        &setup.auth_protocol_data,
    ) {
        warn!(
            "client {} auth rejected ({reason:?}); presented proto {:?}",
            id.0,
            String::from_utf8_lossy(&setup.auth_protocol_name)
        );
        x11::write_setup_failed(&mut stream, setup.byte_order, reason)?;
        return Ok(()); // drop stream → connection closes
    }

    // Sync rendezvous with the core: allocate ids + snapshot geometry.
    let (response_tx, response_rx) = bounded::<SetupAllocateResponse>(1);
    sender.send(Message::SetupAllocate {
        id,
        response_tx: response_tx.clone(),
    })?;
    let resp = response_rx
        .recv()
        .map_err(|_| io::Error::other("core dropped before SetupAllocate response"))?;

    if resp.resource_id_base == 0 {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest exhausted resource ID space",
        )?;
        return Ok(());
    }

    debug!(
        "client {} setup: protocol {}.{}, base=0x{:x}",
        id.0, setup.protocol_major, setup.protocol_minor, resp.resource_id_base
    );

    // mm = px * 25.4 / 96 (integer form: (px*254 + 480) / 960).
    // Matches Xorg's and Xwayland's SETUP-reply convention: 5120 px →
    // 1354 mm, 1440 px → 381 mm. Matches xts5's tetexec.cfg
    // XT_WIDTH_MM/XT_HEIGHT_MM expectations. Real per-monitor mm
    // (from EDID via DRM connector) is reported separately via RANDR
    // GetOutputInfo.
    let screen_width_mm = ((u32::from(resp.screen_width_px) * 254 + 480) / 960)
        .max(1)
        .min(u32::from(u16::MAX)) as u16;
    let screen_height_mm = ((u32::from(resp.screen_height_px) * 254 + 480) / 960)
        .max(1)
        .min(u32::from(u16::MAX)) as u16;

    x11::write_setup_success(
        &mut stream,
        setup.byte_order,
        x11::SetupSuccess {
            protocol_major: setup.protocol_major,
            protocol_minor: setup.protocol_minor,
            // Mirror a recent X.Org release (1.24.1.1 = 12401011).
            // xts5 Xlib3.XVendorRelease keys on this; many apps use it
            // for X.Org-specific compat workarounds.
            release_number: 12_401_011,
            resource_id_base: resp.resource_id_base,
            resource_id_mask: resp.resource_id_mask,
            motion_buffer_size: 0,
            maximum_request_length: u16::MAX,
            image_byte_order: setup.byte_order,
            bitmap_format_bit_order: setup.byte_order,
            bitmap_format_scanline_unit: 32,
            bitmap_format_scanline_pad: 32,
            min_keycode: 8,
            max_keycode: 255,
            // xts5 Xlib3.XServerVendor expects this exact string. Real
            // X.Org reports the same — using it here keeps Xlib clients
            // that key off vendor for compat workarounds happy.
            vendor: "The X.Org Foundation",
            root: x11::Screen {
                root: ROOT_WINDOW,
                default_colormap: ROOT_COLORMAP,
                white_pixel: 0x00ff_ffff,
                black_pixel: 0,
                current_input_masks: resp.current_input_masks,
                width_px: resp.screen_width_px,
                height_px: resp.screen_height_px,
                width_mm: screen_width_mm,
                height_mm: screen_height_mm,
                min_installed_maps: 1,
                max_installed_maps: 1,
                root_visual: ROOT_VISUAL,
                argb_visual: ARGB_VISUAL,
                root_depth: 24,
            },
        },
    )?;

    // Clear timeouts before handing the stream to the core; the reader
    // thread (C3) treats EAGAIN as "wait on poll(2) and retry", but
    // SO_RCVTIMEO would still fire on a quiet client.
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    sender.send(Message::ClientSetupComplete {
        id,
        // The connection's generation, fixed at accept — handed on so
        // the reader thread the core spawns for this client inherits it.
        generation: sender.generation(),
        stream,
        resource_id_base: resp.resource_id_base,
        resource_id_mask: resp.resource_id_mask,
        byte_order: setup.byte_order,
        is_local,
        fd_passing,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core_loop::sender::channel, xauth::MIT_MAGIC_COOKIE};
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        path::PathBuf,
        time::Instant,
    };
    use yserver_protocol::x11::ClientByteOrder;

    #[test]
    fn tcp_setup_without_loaded_auth_never_allocates_a_client() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (_poll, sender, rx) = channel().unwrap();
        let registry = make_registry();
        spawn(
            ClientId(1),
            Transport::Tcp(stream),
            sender.bind(),
            registry.clone(),
            AuthState::new(None),
            false,
            false,
        )
        .unwrap();
        let mut setup = [0; 12];
        setup[0] = b'l';
        setup[2] = 11;
        peer.write_all(&setup).unwrap();
        let mut reply = [0; 8];
        let result = peer.read_exact(&mut reply);
        let allocated = rx
            .try_recv_all()
            .any(|m| matches!(m, Message::SetupAllocate { .. }));
        shutdown_all(&registry);
        assert!(!allocated, "TCP must not use Unix's fail-open auth path");
        result.unwrap();
        assert_eq!(reply[0], 0, "setup refused");
    }

    /// Hand-encode a minimal little-endian SetupRequest with empty auth.
    fn write_setup_request(s: &mut UnixStream) -> io::Result<()> {
        let mut buf = [0u8; 12];
        buf[0] = b'l'; // little-endian
        buf[1] = 0;
        // protocol_major = 11
        buf[2] = 11;
        buf[3] = 0;
        // protocol_minor = 0
        buf[4] = 0;
        buf[5] = 0;
        // auth_name_len = 0
        buf[6] = 0;
        buf[7] = 0;
        // auth_data_len = 0
        buf[8] = 0;
        buf[9] = 0;
        // pad
        s.write_all(&buf)
    }

    fn write_big_endian_setup(s: &mut UnixStream) -> io::Result<()> {
        let mut buf = [0u8; 12];
        buf[0] = b'B';
        buf[2] = 0; // protocol_major hi
        buf[3] = 11; // lo (big-endian)
        s.write_all(&buf)
    }

    /// Drain `n` bytes from `s` with a deadline; returns the bytes.
    fn read_n_with_timeout(s: &mut UnixStream, n: usize, timeout: Duration) -> io::Result<Vec<u8>> {
        s.set_read_timeout(Some(timeout))?;
        let mut buf = vec![0u8; n];
        s.read_exact(&mut buf)?;
        Ok(buf)
    }

    #[test]
    fn full_handshake_round_trip() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll; // keep the poll alive (waker borrows from registry)
        let registry = make_registry();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let id = ClientId(7);

        spawn(
            id,
            server_side,
            sender.bind(),
            registry.clone(),
            AuthState::new(None),
            true,
            true,
        )
        .unwrap();

        // Client writes SetupRequest.
        write_setup_request(&mut client_side).unwrap();

        // Core sees SetupAllocate first.
        let alloc_msg = wait_for_message(&rx, Duration::from_secs(2)).unwrap();
        let response_tx = match alloc_msg {
            Message::SetupAllocate {
                id: got_id,
                response_tx,
            } => {
                assert_eq!(got_id, id);
                response_tx
            }
            other => panic!("expected SetupAllocate, got {other:?}"),
        };

        // Core replies with valid allocation.
        response_tx
            .send(SetupAllocateResponse {
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                screen_width_px: 800,
                screen_height_px: 600,
                current_input_masks: 0,
            })
            .unwrap();

        // Client reads setup_success — first byte is success code 1.
        let head = read_n_with_timeout(&mut client_side, 8, Duration::from_secs(2)).unwrap();
        assert_eq!(head[0], 1, "first byte should be setup_success code");

        // Continue draining whatever the setup_success body is so the
        // setup thread's write completes; the exact length is encoded
        // in head[6..8] (additional_data length in 4-byte units).
        let extra_words = u16::from_le_bytes([head[6], head[7]]);
        let extra_bytes = (extra_words as usize) * 4;
        if extra_bytes > 0 {
            let _ = read_n_with_timeout(&mut client_side, extra_bytes, Duration::from_secs(2));
        }

        // Core sees ClientSetupComplete.
        let complete = wait_for_message(&rx, Duration::from_secs(2)).unwrap();
        match complete {
            Message::ClientSetupComplete {
                id: got,
                resource_id_base,
                resource_id_mask,
                byte_order,
                ..
            } => {
                assert_eq!(got, id);
                assert_eq!(resource_id_base, 0x0010_0000);
                assert_eq!(resource_id_mask, 0x000F_FFFF);
                assert_eq!(byte_order, ClientByteOrder::LittleEndian);
            }
            other => panic!("expected ClientSetupComplete, got {other:?}"),
        }

        // Registry entry was removed by the Drop guard once the
        // thread finished. Allow a brief moment for the thread to
        // actually exit after the message send.
        wait_until(Duration::from_secs(2), || {
            registry.lock().unwrap().is_empty()
        });
        assert!(registry.lock().unwrap().is_empty());
    }

    #[test]
    fn big_endian_setup_completes_with_be_encoded_reply() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let registry = make_registry();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let id = ClientId(11);

        spawn(
            id,
            server_side,
            sender.bind(),
            registry.clone(),
            AuthState::new(None),
            true,
            true,
        )
        .unwrap();
        write_big_endian_setup(&mut client_side).unwrap();

        // Core sees SetupAllocate.
        let alloc_msg = wait_for_message(&rx, Duration::from_secs(2)).unwrap();
        let response_tx = match alloc_msg {
            Message::SetupAllocate { response_tx, .. } => response_tx,
            other => panic!("expected SetupAllocate, got {other:?}"),
        };
        response_tx
            .send(SetupAllocateResponse {
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                screen_width_px: 800,
                screen_height_px: 600,
                current_input_masks: 0,
            })
            .unwrap();

        // Reply header: success(1), pad, protocol_major (BE u16),
        // protocol_minor (BE u16), additional_data length (BE u16).
        let head = read_n_with_timeout(&mut client_side, 8, Duration::from_secs(2)).unwrap();
        assert_eq!(head[0], 1, "first byte should be setup_success code");
        // protocol_major was 11 — BE encodes it as [0x00, 0x0b] at head[2..4].
        assert_eq!(head[2..4], [0x00, 0x0b]);
        // additional_data length lives at head[6..8] in BE.
        let extra_words = u16::from_be_bytes([head[6], head[7]]);
        let extra_bytes = (extra_words as usize) * 4;
        if extra_bytes > 0 {
            let _ = read_n_with_timeout(&mut client_side, extra_bytes, Duration::from_secs(2));
        }

        // Core sees ClientSetupComplete with BigEndian byte_order.
        let complete = wait_for_message(&rx, Duration::from_secs(2)).unwrap();
        match complete {
            Message::ClientSetupComplete { byte_order, .. } => {
                assert_eq!(byte_order, ClientByteOrder::BigEndian);
            }
            other => panic!("expected ClientSetupComplete, got {other:?}"),
        }

        wait_until(Duration::from_secs(2), || {
            registry.lock().unwrap().is_empty()
        });
        assert!(registry.lock().unwrap().is_empty());
    }

    #[test]
    fn shutdown_unblocks_slow_client() {
        let (poll, sender, _rx) = channel().unwrap();
        let _ = poll;
        let registry = make_registry();
        let (server_side, _client_side) = UnixStream::pair().unwrap();
        let id = ClientId(17);

        spawn(
            id,
            server_side,
            sender.bind(),
            registry.clone(),
            AuthState::new(None),
            true,
            true,
        )
        .unwrap();

        // Peer never sends bytes; the setup thread is blocked in
        // read_setup_request. Trigger shutdown.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !registry.lock().unwrap().is_empty(),
            "thread should be parked in read"
        );
        shutdown_all(&registry);

        wait_until(Duration::from_secs(2), || {
            registry.lock().unwrap().is_empty()
        });
        assert!(
            registry.lock().unwrap().is_empty(),
            "thread should have exited after shutdown_all"
        );
    }

    #[test]
    fn shutdown_during_setup_success_write_unblocks_thread() {
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let registry = make_registry();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        let id = ClientId(23);

        spawn(
            id,
            server_side,
            sender.bind(),
            registry.clone(),
            AuthState::new(None),
            true,
            true,
        )
        .unwrap();
        write_setup_request(&mut client_side).unwrap();

        let response_tx = match wait_for_message(&rx, Duration::from_secs(2)).unwrap() {
            Message::SetupAllocate { response_tx, .. } => response_tx,
            other => panic!("expected SetupAllocate, got {other:?}"),
        };
        response_tx
            .send(SetupAllocateResponse {
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                screen_width_px: 800,
                screen_height_px: 600,
                current_input_masks: 0,
            })
            .unwrap();

        // Peer never reads — leave the kernel buffer to fill mid-write.
        // The setup_success body fits in default SO_SNDBUF, so the
        // write may complete; what we really test is that an explicit
        // shutdown() unblocks the thread regardless. Drop the peer to
        // induce EPIPE.
        drop(client_side);

        wait_until(Duration::from_secs(2), || {
            registry.lock().unwrap().is_empty()
        });
        assert!(registry.lock().unwrap().is_empty());
    }

    fn try_recv(rx: &crate::core_loop::sender::CoreReceiver) -> Option<Message> {
        rx.try_recv_all().next()
    }

    fn wait_for_message(
        rx: &crate::core_loop::sender::CoreReceiver,
        timeout: Duration,
    ) -> Option<Message> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(m) = try_recv(rx) {
                return Some(m);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Hand-encode a SetupRequest with a MIT-MAGIC-COOKIE-1 auth payload.
    fn write_setup_with_cookie(s: &mut UnixStream, cookie: &[u8]) -> io::Result<()> {
        let name = MIT_MAGIC_COOKIE.as_bytes();
        let mut buf = Vec::new();
        buf.push(b'l'); // little-endian
        buf.push(0);
        buf.extend_from_slice(&11u16.to_le_bytes()); // protocol major
        buf.extend_from_slice(&0u16.to_le_bytes()); // protocol minor
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(&(cookie.len() as u16).to_le_bytes());
        buf.extend_from_slice(&[0, 0]); // 2 pad bytes (header is 12)
        // name + pad4
        buf.extend_from_slice(name);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
        // data + pad4
        buf.extend_from_slice(cookie);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
        s.write_all(&buf)
    }

    fn xauth_file(tag: &str, cookie: &[u8]) -> PathBuf {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&256u16.to_be_bytes()); // FamilyLocal
        for f in [
            b"host".as_slice(),
            b"0",
            MIT_MAGIC_COOKIE.as_bytes(),
            cookie,
        ] {
            bytes.extend_from_slice(&(f.len() as u16).to_be_bytes());
            bytes.extend_from_slice(f);
        }
        let path =
            std::env::temp_dir().join(format!("yserver-setup-auth-{}-{tag}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn matching_cookie_reaches_core() {
        let cookie = [0x5Au8; 16];
        let path = xauth_file("ok", &cookie);
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let registry = make_registry();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        spawn(
            ClientId(1),
            server_side,
            sender.bind(),
            registry.clone(),
            AuthState::new(Some(path.clone())),
            true,
            true,
        )
        .unwrap();

        write_setup_with_cookie(&mut client_side, &cookie).unwrap();

        // Allowed → core receives SetupAllocate.
        match wait_for_message(&rx, Duration::from_secs(2)).unwrap() {
            Message::SetupAllocate { .. } => {}
            other => panic!("expected SetupAllocate, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bogus_cookie_is_refused_on_the_wire() {
        let path = xauth_file("bad", &[0x11u8; 16]);
        let (poll, sender, rx) = channel().unwrap();
        let _ = poll;
        let registry = make_registry();
        let (server_side, mut client_side) = UnixStream::pair().unwrap();
        spawn(
            ClientId(2),
            server_side,
            sender.bind(),
            registry.clone(),
            AuthState::new(Some(path.clone())),
            true,
            true,
        )
        .unwrap();

        write_setup_with_cookie(&mut client_side, &[0x22u8; 16]).unwrap(); // wrong cookie

        // Rejected → first reply byte is 0 (Failed), and a reason follows.
        let head = read_n_with_timeout(&mut client_side, 8, Duration::from_secs(2)).unwrap();
        assert_eq!(head[0], 0, "setup-failed reply");
        let reason_len = head[1] as usize;
        let length_units = u16::from_le_bytes([head[6], head[7]]) as usize;
        assert_eq!(
            length_units,
            reason_len.div_ceil(4),
            "length field present & correct"
        );
        let reason =
            read_n_with_timeout(&mut client_side, length_units * 4, Duration::from_secs(2))
                .unwrap();
        assert_eq!(&reason[..reason_len], b"Invalid MIT-MAGIC-COOKIE-1 key");

        // Core must NOT have received a SetupAllocate.
        assert!(
            wait_for_message(&rx, Duration::from_millis(200)).is_none(),
            "rejected client must not reach the core"
        );

        // Connection is closed by the server after reject (read → 0 bytes / EOF).
        let mut tail = [0u8; 1];
        client_side
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = client_side.read(&mut tail).unwrap_or(0);
        assert_eq!(n, 0, "server closed the connection after reject");
        let _ = std::fs::remove_file(&path);
    }
}
