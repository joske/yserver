//! Hardware-free tests of the actual startup listener and core setup paths.

use std::{
    fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    time::Duration,
};

use yserver_core::{
    core_loop::{self, Message, auth::AuthState, poll_tokens::ClientIdAllocator},
    server::ServerState,
    transport::Listener,
    xauth::{MIT_MAGIC_COOKIE, parse_records},
};

const COOKIE: [u8; 16] = [0x59; 16];

struct Fixture {
    directory: PathBuf,
    port: u16,
    // Hold a random unused port until immediately before the production bind.
    reservation: Option<TcpListener>,
}

impl Fixture {
    fn new() -> Self {
        let reservation = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        assert!(port >= 6000);
        let directory =
            std::env::temp_dir().join(format!("yserver-tcp-{}-{port}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        Self {
            directory,
            port,
            reservation: Some(reservation),
        }
    }

    fn auth_path(&self) -> PathBuf {
        self.directory.join("authority")
    }

    fn unix_path(&self) -> PathBuf {
        self.directory.join("X")
    }

    fn address(&self) -> SocketAddr {
        (Ipv4Addr::LOCALHOST, self.port).into()
    }

    fn options(&self, args: &[&str]) -> crate::launch::LaunchOptions {
        crate::launch::parse_args(
            [format!(":{}", self.port - 6000)]
                .into_iter()
                .chain(args.iter().map(|arg| (*arg).to_owned())),
        )
        .unwrap()
    }

    fn write_authority(&self) {
        // FamilyInternet (0), 127.0.0.1, and the actual display. This test
        // selects its own cookie from the file below, so the family only has
        // to be self-consistent — it is NOT a claim about what a real client
        // would pick. Measured 2026-09-09: with only a FamilyLocal record
        // present, Xlib connecting to 127.0.0.1:N sends that cookie anyway,
        // because xtrans converts the loopback address to FamilyLocal. A
        // FamilyInternet record is required for a genuinely remote client,
        // not for loopback.
        let mut record = 0u16.to_be_bytes().to_vec();
        let number = (self.port - 6000).to_string();
        for field in [
            [127, 0, 0, 1].as_slice(),
            number.as_bytes(),
            MIT_MAGIC_COOKIE.as_bytes(),
            COOKIE.as_slice(),
        ] {
            record.extend_from_slice(&(field.len() as u16).to_be_bytes());
            record.extend_from_slice(field);
        }
        fs::write(self.auth_path(), record).unwrap();
    }

    fn bind(
        &mut self,
        opts: &crate::launch::LaunchOptions,
        auth: &AuthState,
    ) -> io::Result<Vec<Listener>> {
        let unix = UnixListener::bind(self.unix_path())?;
        self.reservation.take();
        super::bind_client_listeners(unix, self.port - 6000, opts, auth)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.unix_path());
        let _ = fs::remove_file(self.auth_path());
        let _ = fs::remove_dir(&self.directory);
    }
}

#[test]
fn no_listen_tcp_has_no_bound_tcp_socket() {
    for args in [
        &[][..],
        &["-nolisten", "tcp"][..],
        &["-listen", "tcp", "-nolisten", "tcp"][..],
    ] {
        let mut fixture = Fixture::new();
        let opts = fixture.options(args);
        let auth = AuthState::new(None);
        let listeners = fixture.bind(&opts, &auth).unwrap();
        assert_eq!(listeners.len(), 1);
        assert!(matches!(&listeners[0], Listener::Unix(_)));
        let err = TcpStream::connect_timeout(&fixture.address(), Duration::from_secs(1))
            .expect_err("TCP must not be bound without opt-in");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }
}

#[test]
fn listen_tcp_binds_ipv4_display_port_after_loading_shared_auth() {
    let mut fixture = Fixture::new();
    fixture.write_authority();
    let opts = fixture.options(&[
        "-listen",
        "tcp",
        "-auth",
        fixture.auth_path().to_str().unwrap(),
    ]);
    let auth = AuthState::new(opts.auth_file.clone());
    let listeners = fixture.bind(&opts, &auth).unwrap();
    assert_eq!(listeners.len(), 2, "opt-in must bind the TCP listener");
    let Listener::Tcp(tcp) = &listeners[1] else {
        panic!("missing TCP listener")
    };
    assert_eq!(
        tcp.local_addr().unwrap(),
        (Ipv4Addr::UNSPECIFIED, fixture.port).into()
    );
    TcpStream::connect(fixture.address()).unwrap();
    assert_eq!(
        auth.check(
            core_loop::auth::AuthTransport::Tcp,
            core_loop::generation::Generation::default(),
            MIT_MAGIC_COOKIE.as_bytes(),
            &COOKIE
        ),
        core_loop::auth::AuthVerdict::Allow,
        "the same loaded AuthState must authorize runtime clients"
    );
}

#[test]
fn tcp_bind_refuses_unusable_auth_before_opening_port() {
    for (configured, contents) in [
        (false, None),
        (true, None),
        (true, Some(Vec::new())),
        (true, Some(vec![0, 1, 2])),
    ] {
        let mut fixture = Fixture::new();
        if let Some(bytes) = contents {
            fs::write(fixture.auth_path(), bytes).unwrap();
        }
        let opts = if configured {
            fixture.options(&[
                "-listen",
                "tcp",
                "-auth",
                fixture.auth_path().to_str().unwrap(),
            ])
        } else {
            fixture.options(&["-listen", "tcp"])
        };
        let auth = AuthState::new(opts.auth_file.clone());
        let err = fixture
            .bind(&opts, &auth)
            .err()
            .expect("unusable auth must prevent binding TCP");
        assert!(err.to_string().contains("-auth"));
        assert_eq!(
            TcpStream::connect(fixture.address()).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
    }
}

fn setup(
    stream: &mut (impl Read + Write),
    name: &[u8],
    cookie: &[u8],
) -> io::Result<(u8, Vec<u8>)> {
    let mut request = vec![b'l', 0, 11, 0, 0, 0];
    request.extend_from_slice(&(name.len() as u16).to_le_bytes());
    request.extend_from_slice(&(cookie.len() as u16).to_le_bytes());
    request.extend_from_slice(&[0, 0]);
    for field in [name, cookie] {
        request.extend_from_slice(field);
        request.resize(request.len().next_multiple_of(4), 0);
    }
    stream.write_all(&request)?;
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    let mut body = vec![0; usize::from(u16::from_le_bytes([header[6], header[7]])) * 4];
    stream.read_exact(&mut body)?;
    if header[0] == 0 {
        body.truncate(usize::from(header[1]));
    }
    Ok((header[0], body))
}

fn request_barrier(stream: &mut (impl Read + Write)) -> io::Result<()> {
    // GetInputFocus proves setup handoff, client installation and reader spawn
    // have all completed before the test inspects ClientState at shutdown.
    stream.write_all(&[43, 0, 1, 0])?;
    let mut reply = [0; 32];
    stream.read_exact(&mut reply)?;
    assert_eq!(reply[0], 1);
    Ok(())
}

#[test]
fn tcp_loopback_cookie_setup_and_listener_capabilities() {
    let mut fixture = Fixture::new();
    fixture.write_authority();
    let opts = fixture.options(&[
        "-listen",
        "tcp",
        "-auth",
        fixture.auth_path().to_str().unwrap(),
    ]);
    let auth = AuthState::new(opts.auth_file.clone());
    super::validate_tcp_startup(&opts, &auth).unwrap();
    let listeners = fixture.bind(&opts, &auth).unwrap();
    let records = parse_records(&fs::read(fixture.auth_path()).unwrap());
    let cookie = records
        .iter()
        .find(|record| {
            record.family == 0
                && record.address == [127, 0, 0, 1]
                && record.number == (fixture.port - 6000).to_string().as_bytes()
        })
        .expect("FamilyInternet cookie for this loopback display")
        .data
        .clone();
    let (poll, sender, rx) = core_loop::channel().unwrap();
    let core_sender = sender.clone_handle();
    let handle = std::thread::spawn(move || {
        let mut state = ServerState::new();
        let mut backend = crate::kms::render::KmsBackend::for_tests();
        // The headless fixture's placeholder DRM fd is /dev/null and cannot
        // be polled. Only real socket/completion sources participate here.
        backend.platform.devices.clear();
        let result = core_loop::run_core(
            poll,
            rx,
            core_sender,
            &mut state,
            &mut backend,
            listeners,
            &ClientIdAllocator::new(),
            auth,
            yserver_core::core_loop::ResetPolicy::NoReset,
            None,
        );
        let mut capabilities: Vec<_> = state
            .clients
            .values()
            .map(|client| (client.is_local, client.fd_passing))
            .collect();
        capabilities.sort_unstable();
        (result, capabilities)
    });

    // Keep successful peers alive until after inspecting installed clients.
    let mut peers = Vec::new();
    let mut unix_peer = None;
    let result: io::Result<()> = (|| {
        let mut peer = TcpStream::connect_timeout(&fixture.address(), Duration::from_secs(2))?;
        peer.set_read_timeout(Some(Duration::from_secs(5)))?;
        assert_eq!(setup(&mut peer, MIT_MAGIC_COOKIE.as_bytes(), &cookie)?.0, 1);
        request_barrier(&mut peer)?;
        peers.push(peer);

        let mut unix = UnixStream::connect(fixture.unix_path())?;
        unix.set_read_timeout(Some(Duration::from_secs(5)))?;
        assert_eq!(setup(&mut unix, MIT_MAGIC_COOKIE.as_bytes(), &cookie)?.0, 1);
        request_barrier(&mut unix)?;
        unix_peer = Some(unix);

        for (name, data, reason) in [
            (
                b"".as_slice(),
                b"".as_slice(),
                "Authorization required, but no authorization protocol specified\n",
            ),
            (
                MIT_MAGIC_COOKIE.as_bytes(),
                [0x22; 16].as_slice(),
                "Invalid MIT-MAGIC-COOKIE-1 key",
            ),
        ] {
            let mut peer = TcpStream::connect(fixture.address())?;
            peer.set_read_timeout(Some(Duration::from_secs(5)))?;
            let (status, body) = setup(&mut peer, name, data)?;
            assert_eq!(status, 0, "invalid authorization must be refused");
            assert_eq!(body, reason.as_bytes());
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte)?, 0, "refused connection closes");
        }
        Ok(())
    })();
    sender.send(Message::Shutdown).unwrap();
    let (core_result, capabilities) = handle.join().unwrap();
    core_result.unwrap();
    result.unwrap();
    // (is_local, fd_passing) for the TCP client then the unix one.
    //
    // The TCP peer is 127.0.0.1, so it is LOCAL: `is_local` is an address
    // property, and Xorg's `xtransLocalClient` (os/access.c) answers TRUE
    // for a TCP peer whose address is the server's own. It keeps MIT-SHM,
    // whose legacy `Attach` passes a SysV shmid rather than a descriptor.
    //
    // `fd_passing` stays false: SCM_RIGHTS is impossible over TCP however
    // local the peer is. Until 2026-09-10 both were derived from the
    // transport and this asserted `(false, false)`, which cost a
    // same-machine XDMCP session its shared memory.
    assert_eq!(capabilities, [(true, false), (true, true)]);
}
