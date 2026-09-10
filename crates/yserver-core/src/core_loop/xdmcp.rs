//! The XDMCP socket, timer and action service — the core loop's half of
//! the display-side protocol.
//!
//! Step 6 of `docs/superpowers/plans/2026-09-09-xdmcp-plan.md`; design in
//! `docs/superpowers/specs/2026-09-09-xdmcp-design.md`. The protocol itself
//! is a pure function in `yserver_protocol::xdmcp::state`; everything with
//! an effect lives here:
//!
//! * one UDP socket in the core poller's own poll set ([`XDMCP_TOKEN`]),
//!   alongside the client listeners — **no new thread**, because the state
//!   machine has to see the generation boundary directly;
//! * the single retransmission/dormancy deadline, which joins the loop's
//!   existing per-iteration poll-timeout computation
//!   ([`XdmcpService::next_deadline`]);
//! * the [`XdmcpAction`] service: sends, cookie install/clear against
//!   [`AuthState`], and the reset/terminate outcomes the loop acts on.
//!
//! Two things the design is emphatic about, both implemented here rather
//! than anywhere more convenient:
//!
//! 1. **The generation for `install_session_cookie` is read at the moment
//!    the `Accept` is processed**, on the core loop — never captured when
//!    the socket was created. Every entry point therefore takes the
//!    `Generation` from its caller's `CoreReceiver::current_generation()`.
//! 2. **`ClearCookie` is serialised against an in-flight setup.** Each
//!    `AuthState` operation is atomic under its mutex, but a setup thread
//!    that has already passed `check` is past that point, so the ordering
//!    comes from the machine's `SessionClientEstablished` event — see
//!    [`XdmcpService::note_client_established`].

use std::{
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

use mio::{Interest, Token, unix::SourceFd};
use yserver_protocol::{
    x11::ClientId,
    xdmcp::{
        InitialMode, PacketDestination, TerminateReason, XDM_MAX_MSGLEN, XdmcpAction, XdmcpConfig,
        XdmcpEvent, XdmcpMachine, XdmcpMessage, XdmcpState, decode_message, encode_message,
    },
};

use super::{Generation, auth::AuthState};

/// The XDMCP socket's poll token. Fixed, like the notify and signal
/// tokens, and below the listener range (`poll_tokens`'s `0x10`) so it can
/// never collide with a listener, a backend fd or a client writer.
pub const XDMCP_TOKEN: Token = Token(4);

/// `FamilyInternet` (`X11/Xdmcp.h`, and `xdmcp.c`'s
/// `XdmcpRegisterConnection` call sites) — the connection type for an
/// IPv4 address in a `Request`.
const FAMILY_INTERNET: u16 = 0;

/// `defaultDisplayClass` (`xdmcp.c:65`).
///
/// Applied **here**, not in the option parser: a default is only
/// meaningful where the packet is built, and an unset `-class` must reach
/// the wire as this string rather than as an empty `ARRAY8`.
pub const DEFAULT_DISPLAY_CLASS: &[u8] = b"MIT-unspecified";

/// Which query mode the options selected, with the manager host still
/// unresolved. The core's own copy of `launch::XdmcpQueryMode` — the
/// binary crate depends on this one, not the other way round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XdmcpMode {
    /// `-query <host>`.
    Query(String),
    /// `-broadcast`.
    Broadcast,
    /// `-indirect <host>`.
    Indirect(String),
}

impl XdmcpMode {
    fn initial(&self) -> InitialMode {
        match self {
            Self::Query(_) => InitialMode::Query,
            Self::Broadcast => InitialMode::Broadcast,
            Self::Indirect(_) => InitialMode::Indirect,
        }
    }

    fn manager_host(&self) -> Option<&str> {
        match self {
            Self::Query(host) | Self::Indirect(host) => Some(host),
            Self::Broadcast => None,
        }
    }
}

/// Everything [`XdmcpService::bind`] needs from the command line, plus the
/// display number the server actually resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XdmcpSetup {
    pub mode: XdmcpMode,
    /// `-port <n>`, already defaulted to `XDM_UDP_PORT` (177) by the parser.
    pub port: u16,
    /// `-from <addr>`: the source address to query from.
    pub from: Option<String>,
    /// `-class <str>`. `None` becomes [`DEFAULT_DISPLAY_CLASS`].
    pub class: Option<String>,
    /// `-displayID <str>`.
    pub display_id: Option<String>,
    /// `-once`.
    pub once: bool,
    /// The resolved display number, for the `Request`/`Manage` packets.
    pub display_number: u16,
}

/// What the core loop must do about the XDMCP machine's decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XdmcpOutcome {
    /// `dispatchException |= DE_RESET` — end this generation and, once the
    /// new one is installed, re-query ([`XdmcpService::restart`]).
    Reset,
    /// `dispatchException |= DE_TERMINATE`, or `XdmcpFatal`.
    Terminate,
}

/// The socket, the machine, and the one deadline.
pub struct XdmcpService {
    machine: XdmcpMachine,
    socket: UdpSocket,
    /// The resolved manager address, `None` under `-broadcast`.
    manager: Option<SocketAddr>,
    /// Where a [`PacketDestination::Broadcast`] packet goes.
    broadcast: SocketAddr,
    /// The single `xdmcp_timer` deadline, as an `Instant` the loop's
    /// poll-timeout computation can `min` with its own.
    deadline: Option<Instant>,
    /// Latched by [`XdmcpAction::ResetGeneration`] / [`XdmcpAction::Terminate`],
    /// taken by the loop at the tail of the iteration.
    outcome: Option<XdmcpOutcome>,
    /// Scratch receive buffer, one `XDM_MAX_MSGLEN` allocation for the
    /// process rather than one per datagram.
    buffer: Vec<u8>,
}

impl XdmcpService {
    /// Bind the UDP socket and build the machine. Does not send anything —
    /// [`Self::start`] does, once the socket is registered with the poller.
    ///
    /// Fails at startup rather than degrading: an unresolvable `-query`
    /// host or an unusable `-from` address is an operator error, and a
    /// server that came up querying nobody would look like a hung manager.
    pub fn bind(setup: &XdmcpSetup) -> io::Result<Self> {
        let manager = match setup.mode.manager_host() {
            Some(host) => Some(resolve_ipv4(host, setup.port)?),
            None => None,
        };
        let broadcast = SocketAddr::new(
            // The limited broadcast address. Xorg enumerates each
            // interface's own broadcast address (`AddBroadcastAddresses`);
            // 255.255.255.255 reaches every manager on the attached links
            // without the SIOCGIFCONF walk, and is what a single-homed thin
            // client — the deployment #121 is about — would have used
            // anyway. Multi-homed broadcast is not a goal here.
            IpAddr::V4(Ipv4Addr::BROADCAST),
            setup.port,
        );

        let bind_ip = match setup.from.as_deref() {
            Some(from) => resolve_ipv4(from, 0)?.ip(),
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };
        // Port 0: the reply port is ours, not 177 — 177 is the manager's
        // (`XDM_UDP_PORT`, `X11/Xdmcp.h:26`).
        let socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0))?;
        socket.set_nonblocking(true)?;
        if matches!(setup.mode, XdmcpMode::Broadcast) {
            socket.set_broadcast(true)?;
        }

        // `XdmcpRegisterConnection` (`xdmcp.c:456`) registers the display's
        // own addresses so the manager knows where to connect back to and
        // whom to authorize. Ours is the address this socket would use to
        // reach the manager.
        let local = local_address_for(&socket, manager.or(Some(broadcast)));
        let (connection_types, connection_addresses) = match local {
            Some(IpAddr::V4(v4)) => (vec![FAMILY_INTERNET], vec![v4.octets().to_vec()]),
            // No usable address: send the Request with an empty connection
            // list rather than a wrong one. The manager will Decline, which
            // is a diagnosable refusal instead of a session nobody can
            // reach.
            _ => {
                log::warn!("xdmcp: no local IPv4 address for the manager; Request carries none");
                (Vec::new(), Vec::new())
            }
        };

        let config = XdmcpConfig {
            initial_mode: setup.mode.initial(),
            once: setup.once,
            display_number: setup.display_number,
            display_class: setup
                .class
                .as_ref()
                .map_or_else(|| DEFAULT_DISPLAY_CLASS.to_vec(), |c| c.as_bytes().to_vec()),
            manufacturer_display_id: setup
                .display_id
                .as_ref()
                .map_or_else(Vec::new, |d| d.as_bytes().to_vec()),
            // XDM-AUTHENTICATION-1 is a non-goal, so nothing is registered
            // and the Query advertises no authentication mode.
            authentication_names: Vec::new(),
            authorization_names: vec![yserver_protocol::xdmcp::MIT_MAGIC_COOKIE_1.to_vec()],
            connection_types,
            connection_addresses,
            manager_address: manager,
        };
        log::info!(
            "xdmcp: {:?} display {} from {} (manager {:?}, once={})",
            setup.mode,
            setup.display_number,
            socket.local_addr()?,
            manager,
            setup.once,
        );
        Ok(Self {
            machine: XdmcpMachine::new(config),
            socket,
            manager,
            broadcast,
            deadline: None,
            outcome: None,
            buffer: vec![0_u8; XDM_MAX_MSGLEN],
        })
    }

    /// Register the socket with the core poller.
    pub fn register(&self, registry: &mio::Registry) -> io::Result<()> {
        let fd = self.socket.as_raw_fd();
        registry.register(&mut SourceFd(&fd), XDMCP_TOKEN, Interest::READABLE)
    }

    /// `XdmcpInit` (`xdmcp.c:600`): send the first query.
    pub fn start(&mut self, auth: &AuthState, generation: Generation) {
        self.feed(XdmcpEvent::Start, auth, generation);
    }

    /// `XdmcpReset` (`xdmcp.c:618`), which the loop calls **after** the new
    /// generation is installed — the cookie the re-query is about to earn
    /// belongs to that generation, not the one that just ended.
    pub fn restart(&mut self, auth: &AuthState, generation: Generation) {
        self.feed(XdmcpEvent::Start, auth, generation);
    }

    /// The single armed deadline, for the loop's poll-timeout computation.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Drain the socket: every datagram becomes one machine event, decoded
    /// or not.
    ///
    /// `receive_packet` (`xdmcp.c:713`) reads exactly one datagram per
    /// readiness because `ospoll` re-arms level-triggered. mio registers
    /// edge-triggered, so stopping before `WouldBlock` risks a queued
    /// datagram with no further wakeup to collect it — draining is the
    /// requirement here, not an optimisation.
    pub fn handle_readable(&mut self, auth: &AuthState, generation: Generation) {
        loop {
            let (len, from) = match self.socket.recv_from(&mut self.buffer) {
                Ok(v) => v,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    // ICMP port-unreachable from a manager that is not
                    // listening surfaces here as ECONNREFUSED on Linux.
                    // Not fatal: the retransmission budget is what decides
                    // when to give up.
                    log::debug!("xdmcp: recv_from: {e}");
                    return;
                }
            };
            let event = match decode_message(&self.buffer[..len]) {
                Ok(message) => {
                    log::debug!("xdmcp: <- {from} {}", describe(&message));
                    XdmcpEvent::Packet { from, message }
                }
                Err(e) => {
                    log::debug!("xdmcp: <- {from} undecodable ({len} bytes): {e}");
                    XdmcpEvent::UndecodablePacket
                }
            };
            self.feed(event, auth, generation);
        }
    }

    /// Fire the timer if its deadline has passed. Called once per loop
    /// iteration, so it fires whether the poll woke on the timeout or on an
    /// unrelated fd.
    pub fn service_timer(&mut self, now: Instant, auth: &AuthState, generation: Generation) {
        let Some(deadline) = self.deadline else {
            return;
        };
        if now < deadline {
            return;
        }
        // Cleared before the event so a handler that arms nothing (the
        // `-once` exhaustion path) does not leave a deadline in the past
        // spinning the poll timeout at zero.
        self.deadline = None;
        self.feed(XdmcpEvent::TimerExpired, auth, generation);
    }

    /// A client completed an authenticated setup (`XdmcpOpenDisplay`,
    /// `xdmcp.c:632`, called from `ClientAuthorized`,
    /// `os/connection.c:581`). Returns whether that client must be dropped.
    ///
    /// This is the design's `ClearCookie`-versus-setup serialisation, and
    /// the reason the lifecycle is a machine event rather than an
    /// integration detail. `AuthState` is atomic per call, but a setup
    /// thread that has already passed `check` is past that point: a
    /// `Refuse` arriving concurrently clears the credential *after* the
    /// client was authorized by it. The machine decides the race —
    /// `open_display` only takes a client that arrives while a `Manage` is
    /// outstanding — and this reports the loser.
    ///
    /// A client is a loser only when it is **remote** and no session is
    /// running: in XDMCP mode a TCP client can only have been authorized by
    /// the session credential (`AuthState::xdmcp`), so one that did not
    /// become the session and has no session to join was admitted by a
    /// credential that is no longer the running session's. Local clients
    /// are unaffected — a unix client is not authorized by the XDMCP cookie
    /// and Xorg's `XdmcpOpenDisplay` simply ignores it.
    #[must_use]
    pub fn note_client_established(
        &mut self,
        client: ClientId,
        is_local: bool,
        auth: &AuthState,
        generation: Generation,
    ) -> bool {
        self.feed(
            XdmcpEvent::SessionClientEstablished(client),
            auth,
            generation,
        );
        let orphaned = !is_local && !self.session_is_live();
        if orphaned {
            log::warn!(
                "xdmcp: dropping remote client {} — no session is running (state {:?}); \
                 its authorization belonged to an offer that was abandoned",
                client.0,
                self.machine.state(),
            );
        }
        orphaned
    }

    /// The client whose disconnect ends the session (`sessionSocket`,
    /// `xdmcp.c:67`), while it is still the session's.
    ///
    /// The loop compares this against `state.clients` once per iteration:
    /// client ids are allocated monotonically and only the one disconnect
    /// funnel removes an entry, so "no longer in `state.clients`" is the
    /// departure, not an inference about it.
    #[must_use]
    pub fn live_session_client(&self) -> Option<ClientId> {
        if self.session_is_live() {
            self.machine.session_client()
        } else {
            None
        }
    }

    /// `XdmcpCloseDisplay` (`xdmcp.c:642`).
    pub fn note_session_client_disconnected(
        &mut self,
        client: ClientId,
        auth: &AuthState,
        generation: Generation,
    ) {
        self.feed(
            XdmcpEvent::SessionClientDisconnected(client),
            auth,
            generation,
        );
    }

    /// Take whatever the machine decided, for the loop to act on at the end
    /// of the iteration.
    pub fn take_outcome(&mut self) -> Option<XdmcpOutcome> {
        self.outcome.take()
    }

    /// The protocol state, for tests and logging.
    #[must_use]
    pub fn state(&self) -> XdmcpState {
        self.machine.state()
    }

    /// The address the socket is actually bound to (the reply port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn session_is_live(&self) -> bool {
        matches!(
            self.machine.state(),
            XdmcpState::RunSession | XdmcpState::AwaitAliveResponse
        )
    }

    fn feed(&mut self, event: XdmcpEvent, auth: &AuthState, generation: Generation) {
        let actions = self.machine.handle(event);
        for action in actions {
            self.apply(action, auth, generation);
        }
    }

    fn apply(&mut self, action: XdmcpAction, auth: &AuthState, generation: Generation) {
        match action {
            XdmcpAction::Send {
                destination,
                message,
            } => self.send(destination, &message),
            XdmcpAction::SetTimer { seconds } => {
                self.deadline = Some(Instant::now() + Duration::from_secs(u64::from(seconds)));
            }
            XdmcpAction::CancelTimer => self.deadline = None,
            XdmcpAction::InstallCookie { name, data } => {
                // `generation` is the one the core loop is running *now*,
                // as the `Accept` is processed — the design's requirement,
                // and the reason this is not read at bind time.
                if auth.install_session_cookie(generation, &name, &data) {
                    log::info!(
                        "xdmcp: session cookie installed ({} bytes) for generation {generation:?}",
                        data.len(),
                    );
                } else {
                    log::warn!("xdmcp: refused to install an unusable session credential");
                }
            }
            XdmcpAction::ClearCookie => {
                auth.clear_session_cookie();
                log::info!("xdmcp: session cookie cleared (offer abandoned)");
            }
            XdmcpAction::ResetGeneration { cause } => {
                log::info!("xdmcp: {cause:?} — resetting the generation and re-querying");
                self.outcome = Some(XdmcpOutcome::Reset);
            }
            XdmcpAction::Terminate(reason) => {
                match &reason {
                    TerminateReason::Fatal { kind, status } => log::error!(
                        "xdmcp: fatal ({kind:?}): {}",
                        String::from_utf8_lossy(status)
                    ),
                    TerminateReason::OneSession { cause } => {
                        log::info!("xdmcp: -once, {cause:?} — terminating");
                    }
                }
                self.outcome = Some(XdmcpOutcome::Terminate);
            }
        }
    }

    fn send(&mut self, destination: PacketDestination, message: &XdmcpMessage) {
        let target = match destination {
            PacketDestination::Manager => {
                let Some(manager) = self.manager else {
                    log::warn!("xdmcp: no manager address configured; dropping {message:?}");
                    return;
                };
                manager
            }
            PacketDestination::Broadcast => self.broadcast,
            PacketDestination::SelectedHost(host) => host,
        };
        let packet = match encode_message(message) {
            Ok(packet) => packet,
            Err(e) => {
                log::warn!("xdmcp: cannot encode {}: {e}", describe(message));
                return;
            }
        };
        match self.socket.send_to(&packet, target) {
            // A short UDP send is not a thing; report it rather than
            // pretending the packet went out whole.
            Ok(n) if n != packet.len() => {
                log::warn!("xdmcp: -> {target} short send ({n}/{})", packet.len());
            }
            Ok(_) => log::debug!("xdmcp: -> {target} {}", describe(message)),
            // Send failures are retransmission's problem, not a fatal one:
            // a link that is down while the manager is unreachable looks
            // exactly like a manager that is not answering.
            Err(e) => log::debug!("xdmcp: -> {target} {}: {e}", describe(message)),
        }
    }
}

/// The message name, for logs that must never carry cookie bytes.
fn describe(message: &XdmcpMessage) -> &'static str {
    match message {
        XdmcpMessage::Query { .. } => "Query",
        XdmcpMessage::BroadcastQuery { .. } => "BroadcastQuery",
        XdmcpMessage::IndirectQuery { .. } => "IndirectQuery",
        XdmcpMessage::Willing { .. } => "Willing",
        XdmcpMessage::Unwilling { .. } => "Unwilling",
        XdmcpMessage::Request { .. } => "Request",
        XdmcpMessage::Accept { .. } => "Accept",
        XdmcpMessage::Decline { .. } => "Decline",
        XdmcpMessage::Manage { .. } => "Manage",
        XdmcpMessage::Refuse { .. } => "Refuse",
        XdmcpMessage::Failed { .. } => "Failed",
        XdmcpMessage::KeepAlive { .. } => "KeepAlive",
        XdmcpMessage::Alive { .. } => "Alive",
    }
}

/// Resolve a host (or literal address) to one IPv4 socket address.
///
/// IPv6 is out of scope for the whole XDMCP stage, consistent with the
/// IPv4-only listener from stage 1, so an AAAA-only host is a startup
/// error rather than a silent no-op.
fn resolve_ipv4(host: &str, port: u16) -> io::Result<SocketAddr> {
    let mut last_err = None;
    // `to_socket_addrs` needs a port; a bare literal address gets one here.
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            for addr in addrs {
                if addr.is_ipv4() {
                    return Ok(addr);
                }
            }
        }
        Err(e) => last_err = Some(e),
    }
    Err(io::Error::new(
        ErrorKind::InvalidInput,
        match last_err {
            Some(e) => format!("cannot resolve {host} to an IPv4 address: {e}"),
            None => format!("cannot resolve {host} to an IPv4 address (IPv6 is not supported)"),
        },
    ))
}

/// The local address this socket would use to reach `peer`.
///
/// A connected UDP socket sends nothing; the kernel just performs the
/// route lookup and binds the source. `XdmcpRegisterConnection` gets the
/// same answer out of `getifaddrs` plus a route decision we would rather
/// not reimplement.
fn local_address_for(socket: &UdpSocket, peer: Option<SocketAddr>) -> Option<IpAddr> {
    // An explicit `-from` already fixed the source address at bind time.
    if let Ok(local) = socket.local_addr()
        && !local.ip().is_unspecified()
    {
        return Some(local.ip());
    }
    let peer = peer?;
    let probe = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).ok()?;
    probe.set_broadcast(true).ok()?;
    probe.connect(peer).ok()?;
    probe.local_addr().ok().map(|addr| addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yserver_protocol::xdmcp::{MIT_MAGIC_COOKIE_1, XDM_MIN_RTX, XDM_RTX_LIMIT};

    use std::sync::Arc;

    use crate::core_loop::{
        GenerationCounter,
        auth::{AuthTransport, AuthVerdict},
    };

    const COOKIE: &[u8] = b"\x01\x02\x03\x04";
    const OTHER_COOKIE: &[u8] = b"\x09\x09\x09\x09";
    const SESSION: u32 = 0xfeed_face;
    const CLIENT: ClientId = ClientId(11);

    /// A manager on loopback. Real UDP, real encode/decode in both
    /// directions: the service is never handed a message it did not parse
    /// off the wire itself.
    struct FakeManager {
        socket: UdpSocket,
    }

    impl FakeManager {
        fn new() -> Self {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            Self { socket }
        }

        fn port(&self) -> u16 {
            self.socket.local_addr().unwrap().port()
        }

        /// Read one datagram, or `None` if nothing arrives within the
        /// timeout. Returns the decoded message and the display's address.
        fn recv(&self) -> Option<(XdmcpMessage, SocketAddr)> {
            let mut buf = [0_u8; XDM_MAX_MSGLEN];
            let (len, from) = self.socket.recv_from(&mut buf).ok()?;
            Some((decode_message(&buf[..len]).unwrap(), from))
        }

        fn expect(&self, what: &str) -> (XdmcpMessage, SocketAddr) {
            self.recv()
                .unwrap_or_else(|| panic!("no {what} arrived from the display"))
        }

        fn send(&self, to: SocketAddr, message: &XdmcpMessage) {
            let packet = encode_message(message).unwrap();
            self.socket.send_to(&packet, to).unwrap();
        }

        fn send_raw(&self, to: SocketAddr, bytes: &[u8]) {
            self.socket.send_to(bytes, to).unwrap();
        }

        /// How many datagrams are still queued. Short timeout: this is only
        /// ever used to assert that the display sent NOTHING more, and the
        /// datagrams it did send are already queued by the time we ask.
        fn drain(&self) -> usize {
            self.socket
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut seen = 0;
            let mut buf = [0_u8; XDM_MAX_MSGLEN];
            while self.socket.recv_from(&mut buf).is_ok() {
                seen += 1;
            }
            self.socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            seen
        }
    }

    fn setup(port: u16, mode: XdmcpMode, once: bool) -> XdmcpSetup {
        XdmcpSetup {
            mode,
            port,
            from: Some("127.0.0.1".into()),
            class: None,
            display_id: None,
            once,
            display_number: 7,
        }
    }

    fn query_setup(port: u16, once: bool) -> XdmcpSetup {
        setup(port, XdmcpMode::Query("127.0.0.1".into()), once)
    }

    fn willing() -> XdmcpMessage {
        XdmcpMessage::Willing {
            authentication_name: Vec::new(),
            hostname: b"fake-dm".to_vec(),
            status: b"willing to manage".to_vec(),
        }
    }

    fn accept(cookie: &[u8]) -> XdmcpMessage {
        XdmcpMessage::Accept {
            session_id: SESSION,
            authentication_name: Vec::new(),
            authentication_data: Vec::new(),
            authorization_name: MIT_MAGIC_COOKIE_1.to_vec(),
            authorization_data: cookie.to_vec(),
        }
    }

    /// The display's socket, from the manager's point of view, plus the
    /// pieces every test drives the service with.
    struct Harness {
        service: XdmcpService,
        manager: FakeManager,
        display: SocketAddr,
        auth: Arc<AuthState>,
        generations: GenerationCounter,
    }

    impl Harness {
        /// Bind, start, and consume the first query. The returned harness
        /// is in `CollectQuery` with the manager knowing where to reply.
        fn started(setup: &XdmcpSetup, manager: FakeManager) -> Self {
            let auth = AuthState::new_with_xdmcp(None, true);
            let generations = GenerationCounter::new();
            let mut service = XdmcpService::bind(setup).unwrap();
            service.start(&auth, generations.current());
            let (query, display) = manager.expect("Query");
            assert!(matches!(query, XdmcpMessage::Query { .. }), "{query:?}");
            Self {
                service,
                manager,
                display,
                auth,
                generations,
            }
        }

        fn now(&self) -> Generation {
            self.generations.current()
        }

        fn pump(&mut self) {
            let generation = self.now();
            self.service.handle_readable(&self.auth, generation);
        }

        /// Fire the armed timer without waiting for it.
        fn fire_timer(&mut self) {
            let deadline = self
                .service
                .next_deadline()
                .expect("a timer must be armed to fire");
            let generation = self.now();
            self.service.service_timer(deadline, &self.auth, generation);
        }

        fn tcp_verdict(&self, cookie: &[u8]) -> AuthVerdict {
            self.auth.check(
                AuthTransport::Tcp,
                self.now(),
                b"MIT-MAGIC-COOKIE-1",
                cookie,
            )
        }
    }

    /// The whole loop the feature exists for, over real UDP:
    /// Query → Willing → Request → Accept → Manage → session → session end
    /// → reset → re-query.
    #[test]
    fn the_happy_path_runs_a_session_and_queries_again() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), false), manager);

        h.manager.send(h.display, &willing());
        h.pump();
        let (request, _) = h.manager.expect("Request");
        let XdmcpMessage::Request {
            display_number,
            authorization_names,
            authentication_name,
            connection_types,
            connection_addresses,
            ..
        } = &request
        else {
            panic!("expected a Request, got {request:?}");
        };
        assert_eq!(*display_number, 7);
        assert_eq!(authorization_names, &vec![MIT_MAGIC_COOKIE_1.to_vec()]);
        assert!(authentication_name.is_empty());
        assert_eq!(connection_types, &vec![FAMILY_INTERNET]);
        assert_eq!(connection_addresses, &vec![vec![127, 0, 0, 1]]);

        // Nothing authorizes a TCP client before the Accept.
        assert!(matches!(h.tcp_verdict(COOKIE), AuthVerdict::Reject(_)));

        h.manager.send(h.display, &accept(COOKIE));
        h.pump();
        let (manage, _) = h.manager.expect("Manage");
        let XdmcpMessage::Manage {
            session_id,
            display_class,
            ..
        } = &manage
        else {
            panic!("expected a Manage, got {manage:?}");
        };
        assert_eq!(*session_id, SESSION);
        // Step 6 applies the class default, not the parser.
        assert_eq!(display_class, DEFAULT_DISPLAY_CLASS);
        assert_eq!(h.tcp_verdict(COOKIE), AuthVerdict::Allow);

        // The manager's session connects: a remote client, authorized by
        // that cookie.
        assert!(
            !h.service
                .note_client_established(CLIENT, false, &h.auth, h.now())
        );
        assert_eq!(h.service.state(), XdmcpState::RunSession);
        assert_eq!(h.service.live_session_client(), Some(CLIENT));
        assert_eq!(h.service.take_outcome(), None);

        // Session end.
        h.service
            .note_session_client_disconnected(CLIENT, &h.auth, h.now());
        assert_eq!(h.service.take_outcome(), Some(XdmcpOutcome::Reset));
        assert_eq!(h.service.live_session_client(), None);

        // The loop crosses the boundary and only then re-queries, so the
        // cookie the next Accept brings belongs to the NEW generation.
        let previous = h.now();
        let next = h.generations.bump();
        h.service.restart(&h.auth, next);
        let (requery, _) = h.manager.expect("second Query");
        assert!(matches!(requery, XdmcpMessage::Query { .. }), "{requery:?}");

        // The previous session's cookie no longer authorizes anyone. Assert
        // the MECHANISM, not just the refusal: the credential is still
        // installed — a setup thread bound to the OLD generation would
        // still match it — and it is the generation binding, not a clear,
        // that refuses a client of the new one.
        assert!(matches!(h.tcp_verdict(COOKIE), AuthVerdict::Reject(_)));
        assert_eq!(
            h.auth
                .check(AuthTransport::Tcp, previous, b"MIT-MAGIC-COOKIE-1", COOKIE),
            AuthVerdict::Allow,
            "the cookie was cleared rather than invalidated by generation"
        );
    }

    /// A `-broadcast` display reaches the machine's broadcast destination.
    /// Bound to loopback, so the datagram goes nowhere — the point is that
    /// the send path resolves and the collection state is right.
    #[test]
    fn broadcast_mode_collects_rather_than_querying_one_manager() {
        let mut service = XdmcpService::bind(&XdmcpSetup {
            from: Some("127.0.0.1".into()),
            ..setup(17_177, XdmcpMode::Broadcast, false)
        })
        .unwrap();
        let auth = AuthState::new_with_xdmcp(None, true);
        service.start(&auth, Generation::default());
        assert_eq!(service.state(), XdmcpState::CollectBroadcastQuery);
        assert!(service.next_deadline().is_some());
    }

    /// A manager that never answers: backs off and gives up at the limit
    /// rather than spinning or wedging.
    #[test]
    fn a_silent_manager_backs_off_and_gives_up_at_the_limit() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), false), manager);

        let mut delays = Vec::new();
        for _ in 0..XDM_RTX_LIMIT {
            let deadline = h.service.next_deadline().expect("a timer stays armed");
            // Round up: the deadline was armed a few microseconds before
            // this line, so the remaining wait is just under the whole
            // second the backoff asked for.
            let remaining = deadline.saturating_duration_since(Instant::now());
            delays.push(u64::try_from(remaining.as_millis()).unwrap().div_ceil(1000));
            h.fire_timer();
        }
        // `XDM_MIN_RTX << timeOutRtx`, capped at `XDM_MAX_RTX` — 2 s
        // doubling to 32 s. The first entry is the timer the initial Query
        // armed, with the budget still at zero.
        assert_eq!(delays, vec![2, 4, 8, 16, 32, 32, 32]);
        assert_eq!(h.service.take_outcome(), Some(XdmcpOutcome::Reset));
        // Bounded, not spinning: one Query per timeout plus the initial one
        // and `XdmcpDeadSession`'s own re-query, and nothing else.
        // The initial Query was consumed by `Harness::started`; what is
        // left is one per timeout plus `XdmcpDeadSession`'s own re-query.
        for _ in 0..XDM_RTX_LIMIT {
            let (query, _) = h.manager.expect("a retransmitted Query");
            assert!(matches!(query, XdmcpMessage::Query { .. }), "{query:?}");
        }
        assert_eq!(h.manager.drain(), 0);
        // And it is armed again for the new generation's query.
        assert!(h.service.next_deadline().is_some());
    }

    /// `-once`, path two: retransmission exhaustion with no session ever
    /// established. The case a happy-path suite skips.
    #[test]
    fn once_terminates_when_the_manager_never_answers() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), true), manager);
        for _ in 0..XDM_RTX_LIMIT {
            h.fire_timer();
        }
        assert_eq!(h.service.take_outcome(), Some(XdmcpOutcome::Terminate));
        assert_eq!(h.service.state(), XdmcpState::Off);
        // Nothing is armed on the way out, so the loop's poll timeout is
        // not pinned at zero by a deadline in the past.
        assert_eq!(h.service.next_deadline(), None);
    }

    /// `-once`, path one: session end.
    #[test]
    fn once_terminates_at_session_end() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), true), manager);
        h.manager.send(h.display, &willing());
        h.pump();
        let _ = h.manager.expect("Request");
        h.manager.send(h.display, &accept(COOKIE));
        h.pump();
        let _ = h.manager.expect("Manage");
        assert!(
            !h.service
                .note_client_established(CLIENT, false, &h.auth, h.now())
        );
        h.service
            .note_session_client_disconnected(CLIENT, &h.auth, h.now());
        assert_eq!(h.service.take_outcome(), Some(XdmcpOutcome::Terminate));
    }

    /// Divergence A, at the socket: an `Unwilling` arriving while the
    /// session runs is ignored and the session survives.
    #[test]
    fn an_unwilling_during_a_session_is_ignored() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), false), manager);
        h.manager.send(h.display, &willing());
        h.pump();
        let _ = h.manager.expect("Request");
        h.manager.send(h.display, &accept(COOKIE));
        h.pump();
        let _ = h.manager.expect("Manage");
        assert!(
            !h.service
                .note_client_established(CLIENT, false, &h.auth, h.now())
        );

        h.manager.send(
            h.display,
            &XdmcpMessage::Unwilling {
                hostname: b"fake-dm".to_vec(),
                status: b"go away".to_vec(),
            },
        );
        h.pump();
        assert_eq!(h.service.state(), XdmcpState::RunSession);
        assert_eq!(h.service.take_outcome(), None);
        assert_eq!(h.tcp_verdict(COOKIE), AuthVerdict::Allow);
    }

    /// Divergence A.2: under `-query`, an `Unwilling` from anywhere but the
    /// configured manager is not ours to act on.
    #[test]
    fn an_unwilling_from_a_stranger_is_ignored_under_query() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), false), manager);

        let stranger = FakeManager::new();
        stranger.send(
            h.display,
            &XdmcpMessage::Unwilling {
                hostname: b"impostor".to_vec(),
                status: b"go away".to_vec(),
            },
        );
        h.pump();
        assert_eq!(h.service.state(), XdmcpState::CollectQuery);
        assert_eq!(h.service.take_outcome(), None);

        // The configured manager still can, and the display gives up.
        h.manager.send(
            h.display,
            &XdmcpMessage::Unwilling {
                hostname: b"fake-dm".to_vec(),
                status: b"go away".to_vec(),
            },
        );
        h.pump();
        assert_eq!(h.service.take_outcome(), Some(XdmcpOutcome::Terminate));
    }

    /// Divergence A.3: under `-broadcast` an unwilling manager does not end
    /// the collection, and a `Willing` from another manager is accepted.
    ///
    /// Loopback stands in for the broadcast domain: the display's collect
    /// state accepts a reply from whoever sends one, which is exactly what
    /// a broadcast reply is.
    #[test]
    fn an_unwilling_does_not_end_a_broadcast_collection() {
        let auth = AuthState::new_with_xdmcp(None, true);
        let mut service = XdmcpService::bind(&XdmcpSetup {
            from: Some("127.0.0.1".into()),
            ..setup(17_177, XdmcpMode::Broadcast, false)
        })
        .unwrap();
        service.start(&auth, Generation::default());
        let display = service.local_addr().unwrap();
        assert_eq!(service.state(), XdmcpState::CollectBroadcastQuery);

        let unwilling_manager = FakeManager::new();
        unwilling_manager.send(
            display,
            &XdmcpMessage::Unwilling {
                hostname: b"busy-dm".to_vec(),
                status: b"no".to_vec(),
            },
        );
        service.handle_readable(&auth, Generation::default());
        assert_eq!(service.state(), XdmcpState::CollectBroadcastQuery);
        assert_eq!(service.take_outcome(), None);

        let willing_manager = FakeManager::new();
        willing_manager.send(display, &willing());
        service.handle_readable(&auth, Generation::default());
        assert_eq!(service.state(), XdmcpState::AwaitRequestResponse);
        let (request, _) = willing_manager.expect("Request");
        assert!(
            matches!(request, XdmcpMessage::Request { .. }),
            "the Request must go to the manager that answered: {request:?}"
        );
    }

    /// Divergence B, end to end: a peer flooding undecodable and irrelevant
    /// datagrams does not hold the retry budget open. Under Xorg's
    /// `timeOutRtx = 0` this loops forever and `-once` never exits.
    #[test]
    fn a_flood_of_rubbish_still_exhausts_the_budget_and_once_terminates() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), true), manager);

        for _ in 0..XDM_RTX_LIMIT {
            // Undecodable: a truncated header, then a wrong-version packet.
            h.manager.send_raw(h.display, &[0, 1, 0]);
            h.manager.send_raw(h.display, &[0, 99, 0, 5, 0, 0]);
            // Decodable, recognised, and irrelevant in CollectQuery.
            h.manager.send(
                h.display,
                &XdmcpMessage::Alive {
                    session_running: 1,
                    session_id: SESSION,
                },
            );
            h.pump();
            assert_eq!(h.service.take_outcome(), None, "rubbish decided something");
            h.fire_timer();
        }
        assert_eq!(h.service.take_outcome(), Some(XdmcpOutcome::Terminate));
        assert_eq!(h.service.state(), XdmcpState::Off);
    }

    /// The abandoned-offer window: between a `Refuse` and the next
    /// `Accept`, the refused offer's cookie must stop authorizing
    /// immediately — not merely once a replacement arrives.
    #[test]
    fn a_refused_offers_cookie_stops_authorizing_at_once() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), false), manager);
        h.manager.send(h.display, &willing());
        h.pump();
        let _ = h.manager.expect("Request");
        h.manager.send(h.display, &accept(COOKIE));
        h.pump();
        let _ = h.manager.expect("Manage");
        assert_eq!(h.tcp_verdict(COOKIE), AuthVerdict::Allow);

        h.manager.send(
            h.display,
            &XdmcpMessage::Refuse {
                session_id: SESSION,
            },
        );
        h.pump();
        let _ = h.manager.expect("resent Request");
        assert!(
            matches!(h.tcp_verdict(COOKIE), AuthVerdict::Reject(_)),
            "the refused offer's cookie still authorizes"
        );

        // A setup that passed `check` before the clear must not end up
        // running a session: the machine decides the race, and the loser is
        // reported for disconnection.
        assert!(
            h.service
                .note_client_established(CLIENT, false, &h.auth, h.now()),
            "a remote client established with no session must be dropped"
        );
        // `StartConnection` sends the Request as it is entered, so the
        // state the machine settles in is `AwaitRequestResponse`.
        assert_eq!(h.service.state(), XdmcpState::AwaitRequestResponse);
        assert_eq!(h.service.live_session_client(), None);

        // A local client is not the XDMCP credential's business.
        assert!(
            !h.service
                .note_client_established(ClientId(12), true, &h.auth, h.now())
        );

        // The next offer installs its own cookie, and only that one works.
        h.manager.send(h.display, &accept(OTHER_COOKIE));
        h.pump();
        let _ = h.manager.expect("second Manage");
        assert_eq!(h.tcp_verdict(OTHER_COOKIE), AuthVerdict::Allow);
        assert!(matches!(h.tcp_verdict(COOKIE), AuthVerdict::Reject(_)));
    }

    /// An `Accept` we cannot use leaves the state alone and installs
    /// nothing, and the retry timer — not an immediate resend — drives.
    #[test]
    fn an_unusable_accept_installs_nothing_and_lets_the_timer_drive() {
        let manager = FakeManager::new();
        let mut h = Harness::started(&query_setup(manager.port(), false), manager);
        h.manager.send(h.display, &willing());
        h.pump();
        let _ = h.manager.expect("Request");

        h.manager.send(
            h.display,
            &XdmcpMessage::Accept {
                session_id: SESSION,
                authentication_name: Vec::new(),
                authentication_data: Vec::new(),
                authorization_name: b"SUN-DES-1".to_vec(),
                authorization_data: COOKIE.to_vec(),
            },
        );
        h.pump();
        assert_eq!(h.service.state(), XdmcpState::AwaitRequestResponse);
        assert!(matches!(h.tcp_verdict(COOKIE), AuthVerdict::Reject(_)));
        assert_eq!(h.manager.drain(), 0, "it resent instead of waiting");

        // The timer still drives, and the budget was never reset by the bad
        // Accept — seven timeouts from the Request, not from the Accept.
        let remaining = h
            .service
            .next_deadline()
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert_eq!(
            u64::try_from(remaining.as_millis()).unwrap().div_ceil(1000),
            u64::from(XDM_MIN_RTX)
        );
    }

    #[test]
    fn an_unresolvable_manager_is_a_startup_error() {
        let Err(err) = XdmcpService::bind(&query_setup_for("no-such-host.invalid")) else {
            panic!("an unresolvable manager host must not bind");
        };
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    fn query_setup_for(host: &str) -> XdmcpSetup {
        XdmcpSetup {
            mode: XdmcpMode::Query(host.into()),
            port: 177,
            from: None,
            class: None,
            display_id: None,
            once: false,
            display_number: 7,
        }
    }

    /// `-class` reaches the wire as given; only its absence defaults.
    #[test]
    fn an_explicit_display_class_is_not_overridden() {
        let manager = FakeManager::new();
        let mut h = Harness::started(
            &XdmcpSetup {
                class: Some("Thin-Client".into()),
                ..query_setup(manager.port(), false)
            },
            manager,
        );
        h.manager.send(h.display, &willing());
        h.pump();
        let _ = h.manager.expect("Request");
        h.manager.send(h.display, &accept(COOKIE));
        h.pump();
        let (manage, _) = h.manager.expect("Manage");
        let XdmcpMessage::Manage { display_class, .. } = &manage else {
            panic!("expected a Manage, got {manage:?}");
        };
        assert_eq!(display_class, b"Thin-Client");
    }
}
