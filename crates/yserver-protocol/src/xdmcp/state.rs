//! The XDMCP display-side state machine, as a pure function.
//!
//! `(state, event) -> (state, actions)`. No socket, no timer, no clock: the
//! caller feeds decoded packets, timer expiries and session-client lifecycle
//! events in, and gets back a list of things to do.
//!
//! Behaviour reference: `/home/jos/Projects/xserver/os/xdmcp.c`. Every
//! transition below is annotated with the function and line it comes from,
//! because six review rounds on the design each turned on a detail of that
//! file that had been summarised rather than read.
//!
//! Design: `docs/superpowers/specs/2026-09-09-xdmcp-design.md`.
//!
//! # Deliberate divergences from `xdmcp.c`
//!
//! Each is argued in the design document; they are collected here so a
//! reader does not have to infer them:
//!
//! 1. **An unusable authorization fails the offer.** Xorg's `recv_accept_msg`
//!    calls `AddLocalHosts()` (`xdmcp.c:1199`) when `XdmcpAddAuthorization`
//!    fails and proceeds to `XDM_MANAGE` anyway. That fallback needs the
//!    host-based access control we deliberately do not implement, so
//!    proceeding would build a session no TCP client could authenticate to.
//!    We stay in `AwaitRequestResponse` instead and let the retry timer run.
//! 2. **An abandoned offer clears the cookie.** Xorg leaves the installed
//!    authorization in place across a `Refuse`; we emit
//!    [`XdmcpAction::ClearCookie`] because our cookie is per-offer.
//! 3. **`-once` does not re-query on the way out.** `XdmcpDeadSession`
//!    (`xdmcp.c:803`) sets `DE_TERMINATE` and then calls `send_packet()`
//!    anyway, because `dispatchException` is processed later. Emitting a
//!    query we have already decided to abandon serves nothing, so under
//!    `-once` the send is suppressed.
//! 4. **A terminated machine goes inert.** Xorg's `XdmcpFatal` calls
//!    `FatalError`, which never returns, so it has no "after". A pure
//!    function does, so [`XdmcpAction::Terminate`] leaves the machine in
//!    [`XdmcpState::Off`], where every later event is a no-op.
//! 5. **Multicast and the chooser are absent.** `XDM_MULTICAST`,
//!    `XDM_COLLECT_MULTICAST_QUERY` and `XDM_AWAIT_USER_INPUT` are non-goals,
//!    so they are not in [`XdmcpState`].
//! 6. **Sends never fail.** `send_request_msg` moves to
//!    `XDM_AWAIT_REQUEST_RESPONSE` only `if (XdmcpFlush(...))`
//!    (`xdmcp.c:1164`); with no I/O here the transition is unconditional.

use std::net::SocketAddr;

use crate::{x11::ClientId, xdmcp::codec::XdmcpMessage};

/// `XDM_MIN_RTX` (`X11/Xdmcp.h:39`) — the first retransmission delay, in
/// seconds.
pub const XDM_MIN_RTX: u32 = 2;
/// `XDM_MAX_RTX` (`X11/Xdmcp.h:40`) — the backoff ceiling, in seconds.
pub const XDM_MAX_RTX: u32 = 32;
/// `XDM_RTX_LIMIT` (`X11/Xdmcp.h:41`) — retransmissions before the session is
/// declared dead.
pub const XDM_RTX_LIMIT: u32 = 7;
/// `XDM_KA_RTX_LIMIT` (`X11/Xdmcp.h:42`) — the lower limit that applies while
/// awaiting `Alive`.
pub const XDM_KA_RTX_LIMIT: u32 = 4;
/// `XDM_DEF_DORMANCY` (`X11/Xdmcp.h:43`) — seconds a running session may go
/// quiet before a `KeepAlive` is sent.
pub const XDM_DEF_DORMANCY: u32 = 3 * 60;

/// The only authorization name our runtime auth layer recognises.
pub const MIT_MAGIC_COOKIE_1: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// The canned status `receive_packet` passes to `XdmcpFatal` for an
/// `Unwilling` packet — `UnwillingMessage` at `xdmcp.c:710`. Note that the
/// status *in the packet* is never read.
pub const UNWILLING_MESSAGE: &[u8] = b"Host unwilling";

/// The protocol state, from the `xdmcp_states` enum at `X11/Xdmcp.h:52-62`,
/// in that enum's own order.
///
/// `XDM_MULTICAST`, `XDM_COLLECT_MULTICAST_QUERY` (IPv6 multicast) and
/// `XDM_AWAIT_USER_INPUT` (the chooser) are omitted as non-goals;
/// `XDM_KEEP_ME_LAST` is a sentinel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XdmcpState {
    /// `XDM_QUERY` — about to unicast a `Query` to the configured manager.
    Query,
    /// `XDM_BROADCAST` — about to broadcast a `BroadcastQuery`.
    Broadcast,
    /// `XDM_INDIRECT` — about to unicast an `IndirectQuery`.
    Indirect,
    /// `XDM_COLLECT_QUERY` — a `Query` is outstanding.
    CollectQuery,
    /// `XDM_COLLECT_BROADCAST_QUERY` — a `BroadcastQuery` is outstanding.
    CollectBroadcastQuery,
    /// `XDM_COLLECT_INDIRECT_QUERY` — an `IndirectQuery` is outstanding.
    CollectIndirectQuery,
    /// `XDM_START_CONNECTION` — about to send a `Request` to the chosen host.
    StartConnection,
    /// `XDM_AWAIT_REQUEST_RESPONSE` — a `Request` is outstanding.
    AwaitRequestResponse,
    /// `XDM_AWAIT_MANAGE_RESPONSE` — a `Manage` is outstanding.
    AwaitManageResponse,
    /// `XDM_MANAGE` — about to send a `Manage`.
    Manage,
    /// `XDM_RUN_SESSION` — a session client is connected.
    RunSession,
    /// `XDM_OFF` — XDMCP is not running. Also where this machine parks after
    /// a [`XdmcpAction::Terminate`]; see divergence 4 in the module docs.
    Off,
    /// `XDM_KEEPALIVE` — about to send a `KeepAlive`.
    KeepAlive,
    /// `XDM_AWAIT_ALIVE_RESPONSE` — a `KeepAlive` is outstanding.
    AwaitAliveResponse,
}

/// The mode `XDM_INIT_STATE` (`xdmcp.c:80`) holds.
///
/// This is a *variable*, not a state: the options assign `XDM_QUERY`,
/// `XDM_BROADCAST` or `XDM_INDIRECT` to it at `xdmcp.c:254`, `:260` and
/// `:276`. There is no `Init` state to return to, which is why reset targets
/// the configured mode — hardcoding `Query` would work under `-query` and
/// silently break `-broadcast` and `-indirect` from the second session on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialMode {
    /// `-query <host>`.
    Query,
    /// `-broadcast`.
    Broadcast,
    /// `-indirect <host>`.
    Indirect,
}

impl InitialMode {
    /// The state `XDM_INIT_STATE` denotes.
    #[must_use]
    pub fn state(self) -> XdmcpState {
        match self {
            Self::Query => XdmcpState::Query,
            Self::Broadcast => XdmcpState::Broadcast,
            Self::Indirect => XdmcpState::Indirect,
        }
    }
}

/// Everything the machine needs that comes from outside the protocol.
///
/// Step 3 of the plan builds this from `XdmcpOptions`; the fields that are
/// not options come from the server (`XdmcpRegisterConnection`,
/// `XdmcpRegisterAuthorizations`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XdmcpConfig {
    /// `XDM_INIT_STATE` (`xdmcp.c:80`).
    pub initial_mode: InitialMode,
    /// `OneSession` (`xdmcp.c:218`), set by `-once`.
    pub once: bool,
    /// `DisplayNumber` (`xdmcp.c:612`).
    pub display_number: u16,
    /// `DisplayClass`, defaulting to `defaultDisplayClass`
    /// (`xdmcp.c:65`, `"MIT-unspecified"`).
    pub display_class: Vec<u8>,
    /// `ManufacturerDisplayID`, from `-displayID`. Empty by default.
    pub manufacturer_display_id: Vec<u8>,
    /// `AuthenticationNames` — the modes we advertise in a `Query`.
    ///
    /// Empty in every shipping configuration: XDM-AUTHENTICATION-1 is a
    /// non-goal, so nothing calls `XdmcpRegisterAuthentication`.
    pub authentication_names: Vec<Vec<u8>>,
    /// `AuthorizationNames` — `["MIT-MAGIC-COOKIE-1"]`.
    pub authorization_names: Vec<Vec<u8>>,
    /// `ConnectionTypes` — the `FamilyInternet`/`FamilyInternet6` values for
    /// the addresses below.
    pub connection_types: Vec<u16>,
    /// `ConnectionAddresses`, parallel to `connection_types`.
    pub connection_addresses: Vec<Vec<u8>>,
}

impl XdmcpConfig {
    /// A configuration with the protocol's own defaults, for a `-query`
    /// display on `display_number`.
    #[must_use]
    pub fn query(display_number: u16) -> Self {
        Self {
            initial_mode: InitialMode::Query,
            once: false,
            display_number,
            display_class: b"MIT-unspecified".to_vec(),
            manufacturer_display_id: Vec::new(),
            authentication_names: Vec::new(),
            authorization_names: vec![MIT_MAGIC_COOKIE_1.to_vec()],
            connection_types: Vec::new(),
            connection_addresses: Vec::new(),
        }
    }
}

/// Where an outgoing packet goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketDestination {
    /// `ManagerAddress` — the host `-query`/`-indirect` named.
    Manager,
    /// Every registered broadcast address (`BroadcastAddresses`,
    /// `xdmcp.c:999`).
    Broadcast,
    /// `req_sockaddr` — the host whose `Willing` we accepted. `Request`,
    /// `Manage` and `KeepAlive` all go here, not to `ManagerAddress`.
    SelectedHost(SocketAddr),
}

/// Why the session is being renewed or the server torn down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenewCause {
    /// The recorded session client disconnected (`XdmcpCloseDisplay`,
    /// `xdmcp.c:642`).
    SessionEnded,
    /// `XDM_KA_RTX_LIMIT` retransmissions of `KeepAlive` went unanswered
    /// (`xdmcp.c:823`).
    KeepAliveTimedOut,
    /// The manager answered `Alive` saying the session is not running
    /// (`xdmcp.c:1333`).
    AliveSaysSessionDead,
    /// `XDM_RTX_LIMIT` retransmissions went unanswered, in any state — so
    /// this fires during a negotiation that never established a session
    /// (`xdmcp.c:826`).
    RetransmissionsExhausted,
}

/// Which `XdmcpFatal` call site this is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatalKind {
    /// `XdmcpFatal("Manager unwilling", …)` (`xdmcp.c:741`).
    ManagerUnwilling,
    /// `XdmcpFatal("Authentication Failure", …)` (`xdmcp.c:1190`).
    AuthenticationFailure,
    /// `XdmcpFatal("Session declined", …)` (`xdmcp.c:1228`).
    SessionDeclined,
    /// `XdmcpFatal("Session failed", …)` (`xdmcp.c:1288`).
    SessionFailed,
}

/// Why the server is stopping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminateReason {
    /// `XdmcpFatal` (`xdmcp.c:1340`) — the server exits with an error and the
    /// manager's status string.
    Fatal {
        kind: FatalKind,
        /// The `ARRAY8` Xorg formats into the fatal message.
        status: Vec<u8>,
    },
    /// `OneSession` — `-once` turns a renew into a clean exit.
    OneSession { cause: RenewCause },
}

/// Something the caller must do. The machine performs no I/O itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XdmcpAction {
    /// Transmit a packet.
    Send {
        destination: PacketDestination,
        message: XdmcpMessage,
    },
    /// Arm the single XDMCP timer (`xdmcp_timer`) for `seconds`. A later
    /// `SetTimer` replaces an earlier one, as `TimerSet` does.
    SetTimer { seconds: u32 },
    /// `TimerCancel(xdmcp_timer)`.
    CancelTimer,
    /// Install the session credential from an `Accept`, replacing any
    /// previous one.
    InstallCookie { name: Vec<u8>, data: Vec<u8> },
    /// Drop the session credential. Emitted when an offer is abandoned
    /// *within* a generation, where the design's generation binding cannot
    /// invalidate it.
    ClearCookie,
    /// `dispatchException |= DE_RESET` — end this server generation.
    ResetGeneration { cause: RenewCause },
    /// `dispatchException |= DE_TERMINATE`, or `FatalError`.
    Terminate(TerminateReason),
}

/// Everything that can move the machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XdmcpEvent {
    /// `XdmcpInit` (`xdmcp.c:600`) at startup, and `XdmcpReset`
    /// (`xdmcp.c:618`) once a new generation is installed: return to the
    /// configured initial mode and send the first query. Both funnel into
    /// `xdmcp_reset` (`:574`).
    Start,
    /// A datagram that decoded cleanly (`receive_packet`, `xdmcp.c:713`).
    Packet {
        /// The sender, which `recv_willing_msg` needs in order to record
        /// `req_sockaddr`.
        from: SocketAddr,
        message: XdmcpMessage,
    },
    /// A datagram arrived but did not decode — wrong version, unhandled
    /// opcode, bad length.
    ///
    /// This exists because `receive_packet` resets `timeOutRtx` at
    /// `xdmcp.c:728`, *before* it reads the header, so even a garbage
    /// datagram cancels the retransmission backoff. Modelling it keeps that
    /// (attacker-controllable) behaviour visible and testable rather than
    /// accidental.
    UndecodablePacket,
    /// The XDMCP timer fired (`XdmcpTimerNotify`, `xdmcp.c:664`).
    TimerExpired,
    /// A client completed an authenticated setup (`XdmcpOpenDisplay`,
    /// `xdmcp.c:632`). This — not any packet — is what enters `RunSession`.
    SessionClientEstablished(ClientId),
    /// A client disconnected (`XdmcpCloseDisplay`, `xdmcp.c:642`).
    SessionClientDisconnected(ClientId),
}

/// The display-side XDMCP state machine.
#[derive(Clone, Debug)]
pub struct XdmcpMachine {
    config: XdmcpConfig,
    state: XdmcpState,
    /// `SessionID` (`xdmcp.c:77`).
    session_id: u32,
    /// `timeOutRtx` (`xdmcp.c:78`).
    timeout_rtx: u32,
    /// `req_sockaddr` (`xdmcp.c:72`) — the host chosen by `XdmcpSelectHost`.
    selected_host: Option<SocketAddr>,
    /// `sessionSocket` (`xdmcp.c:67`). Only this client ending the connection
    /// ends the session.
    session_client: Option<ClientId>,
    /// `AuthenticationName` (`xdmcp.c:425`), which starts as
    /// `noAuthenticationName` — an empty `ARRAY8` — and is only ever moved by
    /// `XdmcpSetAuthentication` matching a *registered* name.
    authentication_name: Vec<u8>,
}

impl XdmcpMachine {
    /// A machine parked in the configured initial mode, having done nothing.
    ///
    /// Feed it [`XdmcpEvent::Start`] to begin, mirroring `XdmcpInit`'s
    /// `state = XDM_INIT_STATE;` followed by `xdmcp_start()`.
    #[must_use]
    pub fn new(config: XdmcpConfig) -> Self {
        let state = config.initial_mode.state();
        Self {
            config,
            state,
            session_id: 0,
            timeout_rtx: 0,
            selected_host: None,
            session_client: None,
            authentication_name: Vec::new(),
        }
    }

    /// The current protocol state.
    #[must_use]
    pub fn state(&self) -> XdmcpState {
        self.state
    }

    /// `SessionID`, as last carried by an `Accept`.
    #[must_use]
    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// `timeOutRtx` — retransmissions since the last packet arrived.
    #[must_use]
    pub fn timeout_rtx(&self) -> u32 {
        self.timeout_rtx
    }

    /// `sessionSocket` — the client whose disconnect ends the session.
    #[must_use]
    pub fn session_client(&self) -> Option<ClientId> {
        self.session_client
    }

    /// The host chosen from a `Willing` (`req_sockaddr`).
    #[must_use]
    pub fn selected_host(&self) -> Option<SocketAddr> {
        self.selected_host
    }

    /// Apply one event.
    #[must_use]
    pub fn handle(&mut self, event: XdmcpEvent) -> Vec<XdmcpAction> {
        // `XdmcpSocketNotify` returns immediately when `state == XDM_OFF`
        // (`xdmcp.c:658`), and with XDMCP off no timer is armed and no
        // socket registered. Off is therefore inert for every event.
        if self.state == XdmcpState::Off {
            return Vec::new();
        }
        match event {
            XdmcpEvent::Start => self.start(),
            // `receive_packet` resets the backoff before it has even looked
            // at the header (`xdmcp.c:728`).
            XdmcpEvent::UndecodablePacket => {
                self.timeout_rtx = 0;
                Vec::new()
            }
            XdmcpEvent::Packet { from, message } => {
                self.timeout_rtx = 0;
                self.receive(from, &message)
            }
            XdmcpEvent::TimerExpired => self.timer_expired(),
            XdmcpEvent::SessionClientEstablished(client) => self.open_display(client),
            XdmcpEvent::SessionClientDisconnected(client) => self.close_display(client),
        }
    }

    // -------------------------------------------------------------------
    // Lifecycle
    // -------------------------------------------------------------------

    /// `xdmcp_reset` (`xdmcp.c:574`), reached from both `XdmcpInit` and
    /// `XdmcpReset`: reset the backoff, drop any armed timer, and send the
    /// first packet of the configured mode.
    fn start(&mut self) -> Vec<XdmcpAction> {
        self.state = self.config.initial_mode.state();
        self.timeout_rtx = 0;
        let mut actions = vec![XdmcpAction::CancelTimer];
        actions.extend(self.send_packet());
        actions
    }

    /// `XdmcpOpenDisplay` (`xdmcp.c:632`).
    ///
    /// The guard is the whole of it: only a setup completing while a `Manage`
    /// is outstanding starts the session, so a `Refuse` that got there first
    /// (taking the state to `AwaitRequestResponse`) leaves this a no-op —
    /// which is the serialisation the design asks for.
    fn open_display(&mut self, client: ClientId) -> Vec<XdmcpAction> {
        if self.state != XdmcpState::AwaitManageResponse {
            return Vec::new();
        }
        self.state = XdmcpState::RunSession;
        self.session_client = Some(client);
        vec![XdmcpAction::SetTimer {
            seconds: XDM_DEF_DORMANCY,
        }]
    }

    /// `XdmcpCloseDisplay` (`xdmcp.c:642`).
    ///
    /// ```c
    /// if ((state != XDM_RUN_SESSION && state != XDM_AWAIT_ALIVE_RESPONSE)
    ///     || sessionSocket != sock)
    ///     return;
    /// ```
    ///
    /// So a display serving several clients does not end its session when a
    /// transient one exits: only the recorded session client counts, and only
    /// while the session is up or a `KeepAlive` for it is outstanding.
    ///
    /// Note what is *not* here: no `TimerCancel` and no `send_packet`. The
    /// re-query happens later, from `XdmcpReset` on the new generation —
    /// which is what makes the design's "run after the new generation is
    /// installed" requirement Xorg-faithful rather than an invention.
    fn close_display(&mut self, client: ClientId) -> Vec<XdmcpAction> {
        let live = matches!(
            self.state,
            XdmcpState::RunSession | XdmcpState::AwaitAliveResponse
        );
        if !live || self.session_client != Some(client) {
            return Vec::new();
        }
        self.session_client = None;
        self.state = self.config.initial_mode.state();
        if self.config.once {
            self.state = XdmcpState::Off;
            return vec![XdmcpAction::Terminate(TerminateReason::OneSession {
                cause: RenewCause::SessionEnded,
            })];
        }
        vec![XdmcpAction::ResetGeneration {
            cause: RenewCause::SessionEnded,
        }]
    }

    // -------------------------------------------------------------------
    // Sending
    // -------------------------------------------------------------------

    /// `send_packet` (`xdmcp.c:766`): send whatever the current state calls
    /// for, then arm the retransmission timer.
    ///
    /// The timer is armed unconditionally — including from states that send
    /// nothing, which is exactly what the `default: break;` arm plus the
    /// unguarded `TimerSet` at `xdmcp.c:794` does.
    fn send_packet(&mut self) -> Vec<XdmcpAction> {
        let mut actions = Vec::new();
        let sent = match self.state {
            XdmcpState::Query => {
                self.state = XdmcpState::CollectQuery;
                Some((
                    PacketDestination::Manager,
                    XdmcpMessage::Query {
                        authentication_names: self.config.authentication_names.clone(),
                    },
                ))
            }
            XdmcpState::Broadcast => {
                self.state = XdmcpState::CollectBroadcastQuery;
                Some((
                    PacketDestination::Broadcast,
                    XdmcpMessage::BroadcastQuery {
                        authentication_names: self.config.authentication_names.clone(),
                    },
                ))
            }
            XdmcpState::Indirect => {
                self.state = XdmcpState::CollectIndirectQuery;
                Some((
                    PacketDestination::Manager,
                    XdmcpMessage::IndirectQuery {
                        authentication_names: self.config.authentication_names.clone(),
                    },
                ))
            }
            XdmcpState::StartConnection => {
                self.state = XdmcpState::AwaitRequestResponse;
                self.selected_host.map(|host| {
                    (
                        PacketDestination::SelectedHost(host),
                        XdmcpMessage::Request {
                            display_number: self.config.display_number,
                            connection_types: self.config.connection_types.clone(),
                            connection_addresses: self.config.connection_addresses.clone(),
                            authentication_name: self.authentication_name.clone(),
                            // `send_request_msg` runs the selected
                            // authentication mode's Generator here
                            // (`xdmcp.c:1115`). We register none, so the data
                            // is the empty ARRAY8 it initialises to.
                            authentication_data: Vec::new(),
                            authorization_names: self.config.authorization_names.clone(),
                            manufacturer_display_id: self.config.manufacturer_display_id.clone(),
                        },
                    )
                })
            }
            XdmcpState::Manage => {
                self.state = XdmcpState::AwaitManageResponse;
                self.selected_host.map(|host| {
                    (
                        PacketDestination::SelectedHost(host),
                        XdmcpMessage::Manage {
                            session_id: self.session_id,
                            display_number: self.config.display_number,
                            display_class: self.config.display_class.clone(),
                        },
                    )
                })
            }
            XdmcpState::KeepAlive => {
                self.state = XdmcpState::AwaitAliveResponse;
                self.selected_host.map(|host| {
                    (
                        PacketDestination::SelectedHost(host),
                        XdmcpMessage::KeepAlive {
                            display_number: self.config.display_number,
                            session_id: self.session_id,
                        },
                    )
                })
            }
            _ => None,
        };
        if let Some((destination, message)) = sent {
            actions.push(XdmcpAction::Send {
                destination,
                message,
            });
        }
        actions.push(XdmcpAction::SetTimer {
            seconds: retransmit_seconds(self.timeout_rtx),
        });
        actions
    }

    // -------------------------------------------------------------------
    // Timers
    // -------------------------------------------------------------------

    /// `XdmcpTimerNotify` (`xdmcp.c:664`): in `RUN_SESSION` the timer is the
    /// dormancy timer and means "send a KeepAlive"; anywhere else it is the
    /// retransmission timer.
    fn timer_expired(&mut self) -> Vec<XdmcpAction> {
        if self.state == XdmcpState::RunSession {
            self.state = XdmcpState::KeepAlive;
            return self.send_packet();
        }
        self.timeout()
    }

    /// `timeout` (`xdmcp.c:819`).
    fn timeout(&mut self) -> Vec<XdmcpAction> {
        self.timeout_rtx += 1;
        if self.state == XdmcpState::AwaitAliveResponse && self.timeout_rtx >= XDM_KA_RTX_LIMIT {
            return self.dead_session(RenewCause::KeepAliveTimedOut);
        }
        if self.timeout_rtx >= XDM_RTX_LIMIT {
            // `-once` is checked *here*, before `XdmcpDeadSession` is ever
            // reached (`xdmcp.c:826-834`), and this branch neither cancels
            // the timer nor sends anything. That is the case with no session
            // in it: a `-once` server that cannot reach its manager exits
            // rather than looping.
            if self.config.once {
                self.state = XdmcpState::Off;
                return vec![XdmcpAction::Terminate(TerminateReason::OneSession {
                    cause: RenewCause::RetransmissionsExhausted,
                })];
            }
            return self.dead_session(RenewCause::RetransmissionsExhausted);
        }
        // `xdmcp.c:841-854` retries the next resolved manager address for the
        // two unicast collect states. That is DNS bookkeeping outside this
        // function; the state transitions below are the protocol part.
        self.state = match self.state {
            XdmcpState::CollectQuery => XdmcpState::Query,
            XdmcpState::CollectBroadcastQuery => XdmcpState::Broadcast,
            XdmcpState::CollectIndirectQuery => XdmcpState::Indirect,
            XdmcpState::AwaitRequestResponse => XdmcpState::StartConnection,
            XdmcpState::AwaitManageResponse => XdmcpState::Manage,
            XdmcpState::AwaitAliveResponse => XdmcpState::KeepAlive,
            other => other,
        };
        self.send_packet()
    }

    /// `XdmcpDeadSession` (`xdmcp.c:803`).
    ///
    /// ```c
    /// state = XDM_INIT_STATE;
    /// isItTimeToYield = TRUE;
    /// dispatchException |= (OneSession ? DE_TERMINATE : DE_RESET);
    /// TimerCancel(xdmcp_timer);
    /// timeOutRtx = 0;
    /// send_packet();
    /// ```
    ///
    /// `state = XDM_INIT_STATE` is the reset target, and it is the
    /// *configured* mode — see [`InitialMode`]. Under `-once` the trailing
    /// `send_packet()` is suppressed; see divergence 3 in the module docs.
    fn dead_session(&mut self, cause: RenewCause) -> Vec<XdmcpAction> {
        self.state = self.config.initial_mode.state();
        self.timeout_rtx = 0;
        self.session_client = None;
        let mut actions = vec![XdmcpAction::CancelTimer];
        if self.config.once {
            self.state = XdmcpState::Off;
            actions.push(XdmcpAction::Terminate(TerminateReason::OneSession {
                cause,
            }));
            return actions;
        }
        actions.push(XdmcpAction::ResetGeneration { cause });
        actions.extend(self.send_packet());
        actions
    }

    fn fatal(&mut self, kind: FatalKind, status: Vec<u8>) -> Vec<XdmcpAction> {
        self.state = XdmcpState::Off;
        vec![XdmcpAction::Terminate(TerminateReason::Fatal {
            kind,
            status,
        })]
    }

    // -------------------------------------------------------------------
    // Receiving
    // -------------------------------------------------------------------

    /// `receive_packet`'s opcode switch (`xdmcp.c:736-756`).
    ///
    /// Only seven opcodes have a case. `Query`, `BroadcastQuery`,
    /// `IndirectQuery`, `Request`, `Manage` and `KeepAlive` are the display's
    /// own outgoing packets and are silently dropped if one arrives.
    fn receive(&mut self, from: SocketAddr, message: &XdmcpMessage) -> Vec<XdmcpAction> {
        match message {
            XdmcpMessage::Willing {
                authentication_name,
                ..
            } => self.recv_willing(from, authentication_name),
            // No state guard, no length check, and the packet's own status is
            // never read — `receive_packet` passes the canned
            // `UnwillingMessage` (`xdmcp.c:740-742`). An `Unwilling` arriving
            // during a live session is therefore fatal too.
            XdmcpMessage::Unwilling { .. } => {
                self.fatal(FatalKind::ManagerUnwilling, UNWILLING_MESSAGE.to_vec())
            }
            XdmcpMessage::Accept {
                session_id,
                authentication_name,
                authentication_data,
                authorization_name,
                authorization_data,
            } => self.recv_accept(
                *session_id,
                authentication_name,
                authentication_data,
                authorization_name,
                authorization_data,
            ),
            XdmcpMessage::Decline {
                status,
                authentication_name,
                authentication_data,
            } => self.recv_decline(status, authentication_name, authentication_data),
            XdmcpMessage::Refuse { session_id } => self.recv_refuse(*session_id),
            XdmcpMessage::Failed { session_id, status } => self.recv_failed(*session_id, status),
            XdmcpMessage::Alive {
                session_running,
                session_id,
            } => self.recv_alive(*session_running, *session_id),
            _ => Vec::new(),
        }
    }

    /// `recv_willing_msg` (`xdmcp.c:1044`).
    ///
    /// The three collect states are distinct and must stay that way. The
    /// switch at `xdmcp.c:1058` calls `XdmcpSelectHost` from
    /// `XDM_COLLECT_QUERY` — "this is my manager" — but `XdmcpAddHost` from
    /// the broadcast and indirect ones, which is "add it to the list of
    /// managers that answered". Collapsing them silently breaks `-broadcast`
    /// and `-indirect`.
    ///
    /// `XdmcpAddHost` (`xdmcp.c:698`) carries the comment
    /// "!!! this routine should be replaced by a routine that adds the host
    /// to the user's host menu. the current version just selects the first
    /// host to respond with willing message", and its body is a bare
    /// `XdmcpSelectHost(from, fromlen, auth_name)`. Since the chooser is a
    /// non-goal, the two arms converge here as they do there — but through
    /// separate states, so a chooser can be added without disturbing
    /// `-query`.
    fn recv_willing(&mut self, from: SocketAddr, authentication_name: &[u8]) -> Vec<XdmcpAction> {
        match self.state {
            XdmcpState::CollectQuery => self.select_host(from, authentication_name),
            XdmcpState::CollectBroadcastQuery | XdmcpState::CollectIndirectQuery => {
                self.add_host(from, authentication_name)
            }
            _ => Vec::new(),
        }
    }

    /// `XdmcpAddHost` (`xdmcp.c:698`) — first willing host wins, pending a
    /// chooser.
    fn add_host(&mut self, from: SocketAddr, authentication_name: &[u8]) -> Vec<XdmcpAction> {
        self.select_host(from, authentication_name)
    }

    /// `XdmcpSelectHost` (`xdmcp.c:681`).
    fn select_host(&mut self, from: SocketAddr, authentication_name: &[u8]) -> Vec<XdmcpAction> {
        self.state = XdmcpState::StartConnection;
        self.selected_host = Some(from);
        self.set_authentication(authentication_name);
        self.send_packet()
    }

    /// `XdmcpSetAuthentication` (`xdmcp.c:430`).
    ///
    /// It walks the *registered* `AuthenticationNames` and adopts the offered
    /// name only on a match; with none registered — our case, since
    /// XDM-AUTHENTICATION-1 is a non-goal — it is a no-op and
    /// `AuthenticationName` stays the empty `noAuthenticationName`.
    fn set_authentication(&mut self, offered: &[u8]) {
        if self
            .config
            .authentication_names
            .iter()
            .any(|name| name == offered)
        {
            self.authentication_name = offered.to_vec();
        }
    }

    /// `XdmcpCheckAuthentication` (`xdmcp.c:890`):
    ///
    /// ```c
    /// return (XdmcpARRAY8Equal(Name, AuthenticationName) &&
    ///         (AuthenticationName->length == 0 ||
    ///          (*AuthenticationFuncs->Validator)(AuthenticationData, Data, packet_type)));
    /// ```
    ///
    /// Two consequences, both easy to get backwards:
    ///
    /// * With our configured name empty, the array-equal test reduces to
    ///   "the offered name must be empty too".
    /// * The `length == 0` short-circuit then means the accompanying **data
    ///   is never examined**. An empty name with non-empty data is
    ///   *accepted*. An earlier draft of the design required both to be
    ///   empty; that is a divergence with nothing forcing it.
    fn check_authentication(&self, name: &[u8], _data: &[u8]) -> bool {
        if name != self.authentication_name.as_slice() {
            return false;
        }
        // A non-empty configured name would need a Validator, and we register
        // no `AuthenticationFuncs`. Unreachable while XDM-AUTHENTICATION-1 is
        // a non-goal; fail closed if it ever is not.
        self.authentication_name.is_empty()
    }

    /// `recv_accept_msg` (`xdmcp.c:1168`). The first untrusted input in the
    /// flow: anything that can answer our `Query` can send one of these.
    fn recv_accept(
        &mut self,
        session_id: u32,
        authentication_name: &[u8],
        authentication_data: &[u8],
        authorization_name: &[u8],
        authorization_data: &[u8],
    ) -> Vec<XdmcpAction> {
        if self.state != XdmcpState::AwaitRequestResponse {
            return Vec::new();
        }
        // `XdmcpFatal("Authentication Failure", &AcceptAuthenticationName)` —
        // fatal, not a retry, and the status it prints is the offered name.
        if !self.check_authentication(authentication_name, authentication_data) {
            return self.fatal(
                FatalKind::AuthenticationFailure,
                authentication_name.to_vec(),
            );
        }
        // Divergence 1: Xorg falls back to `AddLocalHosts()`; we fail the
        // offer. Both branches below leave the state at
        // `AwaitRequestResponse` so the already-armed retry timer drives —
        // which is also what a malformed `Accept` does in Xorg, since
        // `recv_accept_msg` falls out of its `if` without touching `state`.
        //
        // Non-empty data is load-bearing, not tidiness: our `ct_eq` returns
        // true for two empty slices, so an empty cookie would authorize any
        // client presenting an empty cookie.
        if authorization_name != MIT_MAGIC_COOKIE_1 || authorization_data.is_empty() {
            return vec![XdmcpAction::ClearCookie];
        }
        self.session_id = session_id;
        self.state = XdmcpState::Manage;
        let mut actions = vec![XdmcpAction::InstallCookie {
            name: authorization_name.to_vec(),
            data: authorization_data.to_vec(),
        }];
        actions.extend(self.send_packet());
        actions
    }

    /// `recv_decline_msg` (`xdmcp.c:1213`).
    ///
    /// Note there is **no state guard**: a `Decline` is fatal wherever it
    /// arrives. Note also that a failed authentication check here is silent,
    /// the opposite of `recv_accept_msg`, where it is fatal.
    fn recv_decline(
        &mut self,
        status: &[u8],
        authentication_name: &[u8],
        authentication_data: &[u8],
    ) -> Vec<XdmcpAction> {
        if !self.check_authentication(authentication_name, authentication_data) {
            return Vec::new();
        }
        self.fatal(FatalKind::SessionDeclined, status.to_vec())
    }

    /// `recv_refuse_msg` (`xdmcp.c:1260`).
    ///
    /// ```c
    /// if (state != XDM_AWAIT_MANAGE_RESPONSE)
    ///     return;
    /// ```
    ///
    /// So a late or stray refusal cannot disturb a live session — in
    /// `RunSession` it is ignored outright. The session-id test then makes it
    /// specific to the offer we are actually waiting on.
    fn recv_refuse(&mut self, session_id: u32) -> Vec<XdmcpAction> {
        if self.state != XdmcpState::AwaitManageResponse || session_id != self.session_id {
            return Vec::new();
        }
        self.state = XdmcpState::StartConnection;
        // Divergence 2: the accepted offer is abandoned inside the same
        // generation, so nothing else invalidates its cookie.
        let mut actions = vec![XdmcpAction::ClearCookie];
        actions.extend(self.send_packet());
        actions
    }

    /// `recv_failed_msg` (`xdmcp.c:1277`) — same guard as `Refuse`, but
    /// fatal.
    fn recv_failed(&mut self, session_id: u32, status: &[u8]) -> Vec<XdmcpAction> {
        if self.state != XdmcpState::AwaitManageResponse || session_id != self.session_id {
            return Vec::new();
        }
        self.fatal(FatalKind::SessionFailed, status.to_vec())
    }

    /// `recv_alive_msg` (`xdmcp.c:1317`).
    fn recv_alive(&mut self, session_running: u8, session_id: u32) -> Vec<XdmcpAction> {
        if self.state != XdmcpState::AwaitAliveResponse {
            return Vec::new();
        }
        // `if (SessionRunning && AliveSessionID == SessionID)` — truthiness,
        // not equality with 1.
        if session_running != 0 && session_id == self.session_id {
            self.state = XdmcpState::RunSession;
            return vec![XdmcpAction::SetTimer {
                seconds: XDM_DEF_DORMANCY,
            }];
        }
        self.dead_session(RenewCause::AliveSaysSessionDead)
    }
}

/// `rtx = (XDM_MIN_RTX << timeOutRtx)`, capped at `XDM_MAX_RTX`
/// (`send_packet`, `xdmcp.c:791-793`) — 2 s doubling to 32 s.
#[must_use]
pub fn retransmit_seconds(timeout_rtx: u32) -> u32 {
    match XDM_MIN_RTX.checked_shl(timeout_rtx) {
        // The lower bound also catches a shift that wrapped bits off the top.
        Some(rtx) if (XDM_MIN_RTX..=XDM_MAX_RTX).contains(&rtx) => rtx,
        _ => XDM_MAX_RTX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    const MANAGER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 177);
    const OTHER_MANAGER: SocketAddr =
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 177);
    const SESSION: u32 = 0x1234_5678;
    const COOKIE: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];
    const SESSION_CLIENT: ClientId = ClientId(1);
    const OTHER_CLIENT: ClientId = ClientId(2);

    fn config(mode: InitialMode, once: bool) -> XdmcpConfig {
        XdmcpConfig {
            initial_mode: mode,
            once,
            display_number: 7,
            display_class: b"MIT-unspecified".to_vec(),
            manufacturer_display_id: Vec::new(),
            authentication_names: Vec::new(),
            authorization_names: vec![MIT_MAGIC_COOKIE_1.to_vec()],
            connection_types: vec![0],
            connection_addresses: vec![vec![192, 168, 1, 5]],
        }
    }

    fn machine(mode: InitialMode, once: bool) -> XdmcpMachine {
        XdmcpMachine::new(config(mode, once))
    }

    fn willing() -> XdmcpMessage {
        XdmcpMessage::Willing {
            authentication_name: Vec::new(),
            hostname: b"xdm".to_vec(),
            status: b"willing".to_vec(),
        }
    }

    fn accept_with(name: &[u8], data: &[u8]) -> XdmcpMessage {
        XdmcpMessage::Accept {
            session_id: SESSION,
            authentication_name: Vec::new(),
            authentication_data: Vec::new(),
            authorization_name: name.to_vec(),
            authorization_data: data.to_vec(),
        }
    }

    fn good_accept() -> XdmcpMessage {
        accept_with(MIT_MAGIC_COOKIE_1, &COOKIE)
    }

    fn packet(message: XdmcpMessage) -> XdmcpEvent {
        XdmcpEvent::Packet {
            from: MANAGER,
            message,
        }
    }

    /// Drive a `-query` machine to `AwaitRequestResponse`, discarding the
    /// actions, so a test can start where it means to.
    fn at_await_request_response() -> XdmcpMachine {
        let mut m = machine(InitialMode::Query, false);
        let _ = m.handle(XdmcpEvent::Start);
        let _ = m.handle(packet(willing()));
        assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);
        m
    }

    fn at_await_manage_response() -> XdmcpMachine {
        let mut m = at_await_request_response();
        let _ = m.handle(packet(good_accept()));
        assert_eq!(m.state(), XdmcpState::AwaitManageResponse);
        m
    }

    fn at_run_session() -> XdmcpMachine {
        let mut m = at_await_manage_response();
        let _ = m.handle(XdmcpEvent::SessionClientEstablished(SESSION_CLIENT));
        assert_eq!(m.state(), XdmcpState::RunSession);
        m
    }

    fn at_await_alive_response() -> XdmcpMachine {
        let mut m = at_run_session();
        let _ = m.handle(XdmcpEvent::TimerExpired);
        assert_eq!(m.state(), XdmcpState::AwaitAliveResponse);
        m
    }

    /// Every state, so exhaustive sweeps below can name them all.
    const ALL_STATES: [XdmcpState; 14] = [
        XdmcpState::Query,
        XdmcpState::Broadcast,
        XdmcpState::Indirect,
        XdmcpState::CollectQuery,
        XdmcpState::CollectBroadcastQuery,
        XdmcpState::CollectIndirectQuery,
        XdmcpState::StartConnection,
        XdmcpState::AwaitRequestResponse,
        XdmcpState::AwaitManageResponse,
        XdmcpState::Manage,
        XdmcpState::RunSession,
        XdmcpState::Off,
        XdmcpState::KeepAlive,
        XdmcpState::AwaitAliveResponse,
    ];

    /// Force a machine into an arbitrary state for the exhaustive sweeps,
    /// with the bookkeeping a real arrival there would have left behind.
    fn machine_in(state: XdmcpState, once: bool) -> XdmcpMachine {
        let mut m = machine(InitialMode::Query, once);
        m.state = state;
        m.selected_host = Some(MANAGER);
        m.session_id = SESSION;
        if matches!(
            state,
            XdmcpState::RunSession | XdmcpState::AwaitAliveResponse
        ) {
            m.session_client = Some(SESSION_CLIENT);
        }
        m
    }

    // -----------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------

    /// From `X11/Xdmcp.h:39-43`, not invented.
    #[test]
    fn retransmission_constants_come_from_the_protocol_header() {
        assert_eq!(XDM_MIN_RTX, 2);
        assert_eq!(XDM_MAX_RTX, 32);
        assert_eq!(XDM_RTX_LIMIT, 7);
        assert_eq!(XDM_KA_RTX_LIMIT, 4);
        assert_eq!(XDM_DEF_DORMANCY, 180);
    }

    /// `rtx = XDM_MIN_RTX << timeOutRtx`, capped — 2 s doubling to 32 s.
    #[test]
    fn backoff_doubles_from_two_seconds_and_caps_at_thirty_two() {
        assert_eq!(
            (0..8).map(retransmit_seconds).collect::<Vec<_>>(),
            vec![2, 4, 8, 16, 32, 32, 32, 32]
        );
        // Defensive: a shift that would wrap must not produce a 0 s timer.
        assert_eq!(retransmit_seconds(31), XDM_MAX_RTX);
        assert_eq!(retransmit_seconds(u32::MAX), XDM_MAX_RTX);
    }

    // -----------------------------------------------------------------
    // Start, and the three initial modes
    // -----------------------------------------------------------------

    #[test]
    fn start_in_query_mode_unicasts_a_query_to_the_manager() {
        let mut m = machine(InitialMode::Query, false);
        assert_eq!(m.state(), XdmcpState::Query);
        assert_eq!(
            m.handle(XdmcpEvent::Start),
            vec![
                XdmcpAction::CancelTimer,
                XdmcpAction::Send {
                    destination: PacketDestination::Manager,
                    message: XdmcpMessage::Query {
                        authentication_names: vec![]
                    },
                },
                XdmcpAction::SetTimer { seconds: 2 },
            ]
        );
        assert_eq!(m.state(), XdmcpState::CollectQuery);
    }

    #[test]
    fn start_in_broadcast_mode_broadcasts_and_collects_separately() {
        let mut m = machine(InitialMode::Broadcast, false);
        let actions = m.handle(XdmcpEvent::Start);
        assert!(actions.contains(&XdmcpAction::Send {
            destination: PacketDestination::Broadcast,
            message: XdmcpMessage::BroadcastQuery {
                authentication_names: vec![]
            },
        }));
        assert_eq!(m.state(), XdmcpState::CollectBroadcastQuery);
    }

    #[test]
    fn start_in_indirect_mode_unicasts_an_indirect_query() {
        let mut m = machine(InitialMode::Indirect, false);
        let actions = m.handle(XdmcpEvent::Start);
        assert!(actions.contains(&XdmcpAction::Send {
            destination: PacketDestination::Manager,
            message: XdmcpMessage::IndirectQuery {
                authentication_names: vec![]
            },
        }));
        assert_eq!(m.state(), XdmcpState::CollectIndirectQuery);
    }

    // -----------------------------------------------------------------
    // Willing, and the three distinct collect states
    // -----------------------------------------------------------------

    /// `recv_willing_msg`'s switch (`xdmcp.c:1058`) acts in all three collect
    /// states — via `XdmcpSelectHost` from `CollectQuery` and `XdmcpAddHost`
    /// from the other two — and nowhere else.
    #[test]
    fn willing_is_acted_on_in_every_collect_state_and_ignored_elsewhere() {
        let collect = [
            XdmcpState::CollectQuery,
            XdmcpState::CollectBroadcastQuery,
            XdmcpState::CollectIndirectQuery,
        ];
        for state in ALL_STATES {
            let mut m = machine_in(state, false);
            m.selected_host = None;
            let actions = m.handle(packet(willing()));
            if collect.contains(&state) {
                assert_eq!(
                    m.state(),
                    XdmcpState::AwaitRequestResponse,
                    "{state:?} did not act on Willing"
                );
                assert_eq!(m.selected_host(), Some(MANAGER));
                assert!(matches!(
                    actions.as_slice(),
                    [
                        XdmcpAction::Send {
                            destination: PacketDestination::SelectedHost(_),
                            message: XdmcpMessage::Request { .. },
                        },
                        XdmcpAction::SetTimer { .. },
                    ]
                ));
            } else {
                assert_eq!(m.state(), state, "{state:?} acted on Willing");
                assert!(actions.is_empty(), "{state:?} produced {actions:?}");
                assert_eq!(m.selected_host(), None);
            }
        }
    }

    /// The Request goes to the host that answered, not to the configured
    /// manager — `req_sockaddr`, recorded by `XdmcpSelectHost`. Under
    /// `-broadcast` these differ, which is the whole point of recording it.
    #[test]
    fn the_request_goes_to_the_host_that_answered_willing() {
        let mut m = machine(InitialMode::Broadcast, false);
        let _ = m.handle(XdmcpEvent::Start);
        let actions = m.handle(XdmcpEvent::Packet {
            from: OTHER_MANAGER,
            message: willing(),
        });
        assert!(actions.contains(&XdmcpAction::Send {
            destination: PacketDestination::SelectedHost(OTHER_MANAGER),
            message: XdmcpMessage::Request {
                display_number: 7,
                connection_types: vec![0],
                connection_addresses: vec![vec![192, 168, 1, 5]],
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_names: vec![MIT_MAGIC_COOKIE_1.to_vec()],
                manufacturer_display_id: vec![],
            },
        }));
        assert_eq!(m.selected_host(), Some(OTHER_MANAGER));
    }

    /// `XdmcpSetAuthentication` (`xdmcp.c:430`) only adopts a name that is in
    /// the registered list. We register none, so an offered name is ignored
    /// and the Request still carries the empty `noAuthenticationName`.
    #[test]
    fn an_offered_authentication_name_we_did_not_register_is_ignored() {
        let mut m = machine(InitialMode::Query, false);
        let _ = m.handle(XdmcpEvent::Start);
        let actions = m.handle(packet(XdmcpMessage::Willing {
            authentication_name: b"XDM-AUTHENTICATION-1".to_vec(),
            hostname: b"xdm".to_vec(),
            status: b"willing".to_vec(),
        }));
        let XdmcpAction::Send {
            message:
                XdmcpMessage::Request {
                    authentication_name,
                    ..
                },
            ..
        } = &actions[0]
        else {
            panic!("expected a Request, got {actions:?}");
        };
        assert!(authentication_name.is_empty());
    }

    // -----------------------------------------------------------------
    // Accept
    // -----------------------------------------------------------------

    #[test]
    fn a_good_accept_installs_the_cookie_and_sends_manage() {
        let mut m = at_await_request_response();
        assert_eq!(
            m.handle(packet(good_accept())),
            vec![
                XdmcpAction::InstallCookie {
                    name: MIT_MAGIC_COOKIE_1.to_vec(),
                    data: COOKIE.to_vec(),
                },
                XdmcpAction::Send {
                    destination: PacketDestination::SelectedHost(MANAGER),
                    message: XdmcpMessage::Manage {
                        session_id: SESSION,
                        display_number: 7,
                        display_class: b"MIT-unspecified".to_vec(),
                    },
                },
                XdmcpAction::SetTimer { seconds: 2 },
            ]
        );
        assert_eq!(m.state(), XdmcpState::AwaitManageResponse);
        assert_eq!(m.session_id(), SESSION);
    }

    /// `if (state != XDM_AWAIT_REQUEST_RESPONSE) return;` — `xdmcp.c:1174`.
    #[test]
    fn accept_is_ignored_in_every_state_but_await_request_response() {
        for state in ALL_STATES {
            if state == XdmcpState::AwaitRequestResponse {
                continue;
            }
            let mut m = machine_in(state, false);
            let actions = m.handle(packet(good_accept()));
            assert!(actions.is_empty(), "{state:?} acted on Accept: {actions:?}");
            assert_eq!(m.state(), state);
        }
    }

    /// The fidelity case that is easy to get backwards: with our configured
    /// authentication name empty, `XdmcpCheckAuthentication` short-circuits
    /// on `AuthenticationName->length == 0` and **never looks at the data**.
    /// An empty name with stray data is accepted.
    #[test]
    fn an_empty_authentication_name_with_non_empty_data_is_accepted() {
        let mut m = at_await_request_response();
        let actions = m.handle(packet(XdmcpMessage::Accept {
            session_id: SESSION,
            authentication_name: Vec::new(),
            authentication_data: b"stray bytes".to_vec(),
            authorization_name: MIT_MAGIC_COOKIE_1.to_vec(),
            authorization_data: COOKIE.to_vec(),
        }));
        assert!(actions.contains(&XdmcpAction::InstallCookie {
            name: MIT_MAGIC_COOKIE_1.to_vec(),
            data: COOKIE.to_vec(),
        }));
        assert_eq!(m.state(), XdmcpState::AwaitManageResponse);
    }

    /// A non-empty authentication *name* means the manager selected a mode we
    /// do not implement. `XdmcpFatal("Authentication Failure", …)` — fatal,
    /// not a retry — and the status it carries is the offered name.
    #[test]
    fn a_non_empty_authentication_name_is_fatal() {
        let mut m = at_await_request_response();
        assert_eq!(
            m.handle(packet(XdmcpMessage::Accept {
                session_id: SESSION,
                authentication_name: b"XDM-AUTHENTICATION-1".to_vec(),
                authentication_data: Vec::new(),
                authorization_name: MIT_MAGIC_COOKIE_1.to_vec(),
                authorization_data: COOKIE.to_vec(),
            })),
            vec![XdmcpAction::Terminate(TerminateReason::Fatal {
                kind: FatalKind::AuthenticationFailure,
                status: b"XDM-AUTHENTICATION-1".to_vec(),
            })]
        );
        assert_eq!(m.state(), XdmcpState::Off);
    }

    /// The bullet the plan calls out: an unusable `Accept` leaves the state
    /// **unchanged**, so the already-armed retry timer drives. Not a jump to
    /// `StartConnection` and an immediate resend, which against a manager
    /// that keeps answering badly is a tight loop.
    #[test]
    fn an_unusable_accept_leaves_the_state_at_await_request_response() {
        for (name, data) in [
            (b"MIT-KERBEROS-5".as_slice(), COOKIE.as_slice()),
            (MIT_MAGIC_COOKIE_1, b"".as_slice()),
            (b"".as_slice(), b"".as_slice()),
            (b"mit-magic-cookie-1".as_slice(), COOKIE.as_slice()),
        ] {
            let mut m = at_await_request_response();
            let actions = m.handle(packet(accept_with(name, data)));
            assert_eq!(
                actions,
                vec![XdmcpAction::ClearCookie],
                "name {name:?} data {data:?}"
            );
            assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);
            assert_eq!(m.session_id(), 0, "session id adopted from a bad Accept");
        }
    }

    /// `ct_eq(&[], &[])` is true, so an empty cookie would be a credential
    /// that any client presenting an empty cookie matches. It must never be
    /// installed.
    #[test]
    fn an_empty_cookie_is_never_installed() {
        let mut m = at_await_request_response();
        let actions = m.handle(packet(accept_with(MIT_MAGIC_COOKIE_1, b"")));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, XdmcpAction::InstallCookie { .. }))
        );
        assert_eq!(actions, vec![XdmcpAction::ClearCookie]);
    }

    /// No length requirement on the cookie: `MitAddCookie` memcpys whatever
    /// it is given and our `ct_eq` compares length then bytes. The
    /// conventional 16 bytes is a property of `mcookie`, not the protocol.
    #[test]
    fn a_cookie_of_any_non_empty_length_is_installed() {
        for len in [1usize, 3, 16, 64] {
            let data = vec![0xa5; len];
            let mut m = at_await_request_response();
            let actions = m.handle(packet(accept_with(MIT_MAGIC_COOKIE_1, &data)));
            assert!(actions.contains(&XdmcpAction::InstallCookie {
                name: MIT_MAGIC_COOKIE_1.to_vec(),
                data,
            }));
        }
    }

    // -----------------------------------------------------------------
    // Decline, Unwilling, Failed
    // -----------------------------------------------------------------

    /// `recv_decline_msg` has **no state guard** (`xdmcp.c:1213`).
    #[test]
    fn decline_is_fatal_in_every_state() {
        for state in ALL_STATES {
            if state == XdmcpState::Off {
                continue;
            }
            let mut m = machine_in(state, false);
            assert_eq!(
                m.handle(packet(XdmcpMessage::Decline {
                    status: b"No permission".to_vec(),
                    authentication_name: Vec::new(),
                    authentication_data: Vec::new(),
                })),
                vec![XdmcpAction::Terminate(TerminateReason::Fatal {
                    kind: FatalKind::SessionDeclined,
                    status: b"No permission".to_vec(),
                })],
                "{state:?}"
            );
        }
    }

    /// Unlike `recv_accept_msg`, a failed authentication check in
    /// `recv_decline_msg` is *silent*: the `&&` simply makes the whole
    /// condition false and nothing happens.
    #[test]
    fn a_decline_that_fails_the_authentication_check_is_ignored_not_fatal() {
        let mut m = at_await_request_response();
        assert_eq!(
            m.handle(packet(XdmcpMessage::Decline {
                status: b"No permission".to_vec(),
                authentication_name: b"XDM-AUTHENTICATION-1".to_vec(),
                authentication_data: Vec::new(),
            })),
            vec![]
        );
        assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);
    }

    /// `receive_packet`'s `case UNWILLING:` (`xdmcp.c:740`) has no state
    /// guard, no length check, and ignores the packet's own status in favour
    /// of the canned `UnwillingMessage`.
    #[test]
    fn unwilling_is_fatal_in_every_state_with_the_canned_status() {
        for state in ALL_STATES {
            if state == XdmcpState::Off {
                continue;
            }
            let mut m = machine_in(state, false);
            assert_eq!(
                m.handle(packet(XdmcpMessage::Unwilling {
                    hostname: b"xdm".to_vec(),
                    status: b"a status we must not use".to_vec(),
                })),
                vec![XdmcpAction::Terminate(TerminateReason::Fatal {
                    kind: FatalKind::ManagerUnwilling,
                    status: b"Host unwilling".to_vec(),
                })],
                "{state:?}"
            );
        }
    }

    /// `recv_failed_msg` (`xdmcp.c:1277`): guarded on
    /// `AWAIT_MANAGE_RESPONSE` *and* the session id.
    #[test]
    fn failed_is_fatal_only_in_await_manage_response_and_for_our_session() {
        for state in ALL_STATES {
            let mut m = machine_in(state, false);
            let actions = m.handle(packet(XdmcpMessage::Failed {
                session_id: SESSION,
                status: b"Session failed".to_vec(),
            }));
            if state == XdmcpState::AwaitManageResponse {
                assert_eq!(
                    actions,
                    vec![XdmcpAction::Terminate(TerminateReason::Fatal {
                        kind: FatalKind::SessionFailed,
                        status: b"Session failed".to_vec(),
                    })]
                );
            } else {
                assert!(actions.is_empty(), "{state:?} acted on Failed");
            }
        }
    }

    #[test]
    fn a_failed_for_another_session_is_ignored() {
        let mut m = at_await_manage_response();
        assert_eq!(
            m.handle(packet(XdmcpMessage::Failed {
                session_id: SESSION ^ 1,
                status: b"Session failed".to_vec(),
            })),
            vec![]
        );
        assert_eq!(m.state(), XdmcpState::AwaitManageResponse);
    }

    // -----------------------------------------------------------------
    // Refuse
    // -----------------------------------------------------------------

    /// `if (state != XDM_AWAIT_MANAGE_RESPONSE) return;` (`xdmcp.c:1264`).
    /// In `RunSession` a late refusal must not disturb the live session.
    #[test]
    fn refuse_acts_only_in_await_manage_response() {
        for state in ALL_STATES {
            let mut m = machine_in(state, false);
            let actions = m.handle(packet(XdmcpMessage::Refuse {
                session_id: SESSION,
            }));
            if state == XdmcpState::AwaitManageResponse {
                assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);
                assert_eq!(actions[0], XdmcpAction::ClearCookie);
            } else {
                assert!(actions.is_empty(), "{state:?} acted on Refuse: {actions:?}");
                assert_eq!(m.state(), state);
            }
        }
    }

    #[test]
    fn refuse_clears_the_cookie_and_resends_the_request() {
        let mut m = at_await_manage_response();
        assert_eq!(
            m.handle(packet(XdmcpMessage::Refuse {
                session_id: SESSION
            })),
            vec![
                XdmcpAction::ClearCookie,
                XdmcpAction::Send {
                    destination: PacketDestination::SelectedHost(MANAGER),
                    message: XdmcpMessage::Request {
                        display_number: 7,
                        connection_types: vec![0],
                        connection_addresses: vec![vec![192, 168, 1, 5]],
                        authentication_name: vec![],
                        authentication_data: vec![],
                        authorization_names: vec![MIT_MAGIC_COOKIE_1.to_vec()],
                        manufacturer_display_id: vec![],
                    },
                },
                XdmcpAction::SetTimer { seconds: 2 },
            ]
        );
        assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);
    }

    #[test]
    fn a_refuse_for_another_session_is_ignored() {
        let mut m = at_await_manage_response();
        assert_eq!(
            m.handle(packet(XdmcpMessage::Refuse {
                session_id: SESSION ^ 1
            })),
            vec![]
        );
        assert_eq!(m.state(), XdmcpState::AwaitManageResponse);
    }

    /// The abandoned-offer sequence with no reset in it: `Accept`, `Refuse`,
    /// a second `Accept` with a different cookie. The first cookie must be
    /// gone before the second is installed.
    #[test]
    fn a_refused_offers_cookie_is_cleared_before_the_next_one_is_installed() {
        let mut m = at_await_request_response();
        let first = m.handle(packet(accept_with(MIT_MAGIC_COOKIE_1, b"first")));
        assert_eq!(
            first[0],
            XdmcpAction::InstallCookie {
                name: MIT_MAGIC_COOKIE_1.to_vec(),
                data: b"first".to_vec(),
            }
        );

        let refused = m.handle(packet(XdmcpMessage::Refuse {
            session_id: SESSION,
        }));
        assert_eq!(refused[0], XdmcpAction::ClearCookie);

        let second = m.handle(packet(XdmcpMessage::Accept {
            session_id: SESSION + 1,
            authentication_name: Vec::new(),
            authentication_data: Vec::new(),
            authorization_name: MIT_MAGIC_COOKIE_1.to_vec(),
            authorization_data: b"second".to_vec(),
        }));
        assert_eq!(
            second[0],
            XdmcpAction::InstallCookie {
                name: MIT_MAGIC_COOKIE_1.to_vec(),
                data: b"second".to_vec(),
            }
        );
        assert_eq!(m.session_id(), SESSION + 1);
    }

    // -----------------------------------------------------------------
    // The session lifecycle
    // -----------------------------------------------------------------

    /// `XdmcpOpenDisplay` (`xdmcp.c:632`) — `RunSession` is entered by an
    /// authenticated setup, not by any packet.
    #[test]
    fn only_a_setup_in_await_manage_response_starts_the_session() {
        for state in ALL_STATES {
            let mut m = machine_in(state, false);
            m.session_client = None;
            let actions = m.handle(XdmcpEvent::SessionClientEstablished(SESSION_CLIENT));
            if state == XdmcpState::AwaitManageResponse {
                assert_eq!(m.state(), XdmcpState::RunSession);
                assert_eq!(m.session_client(), Some(SESSION_CLIENT));
                assert_eq!(
                    actions,
                    vec![XdmcpAction::SetTimer {
                        seconds: XDM_DEF_DORMANCY
                    }]
                );
            } else {
                assert_eq!(m.state(), state, "{state:?} started a session");
                assert_eq!(m.session_client(), None);
                assert!(actions.is_empty());
            }
        }
    }

    /// A second client connecting during the session does not become the
    /// session client — the state is no longer `AwaitManageResponse`.
    #[test]
    fn a_second_client_does_not_take_over_the_session_record() {
        let mut m = at_run_session();
        assert_eq!(
            m.handle(XdmcpEvent::SessionClientEstablished(OTHER_CLIENT)),
            vec![]
        );
        assert_eq!(m.session_client(), Some(SESSION_CLIENT));
    }

    /// The rule the design is emphatic about: only the recorded session
    /// client ends the session. On a display serving several clients,
    /// treating any disconnect as the end would reset under the user
    /// whenever a transient client exits.
    #[test]
    fn only_the_recorded_session_client_ends_the_session() {
        let mut m = at_run_session();
        assert_eq!(
            m.handle(XdmcpEvent::SessionClientDisconnected(OTHER_CLIENT)),
            vec![]
        );
        assert_eq!(m.state(), XdmcpState::RunSession);
        assert_eq!(
            m.handle(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT)),
            vec![XdmcpAction::ResetGeneration {
                cause: RenewCause::SessionEnded
            }]
        );
        assert_eq!(m.state(), XdmcpState::Query);
    }

    /// `(state != XDM_RUN_SESSION && state != XDM_AWAIT_ALIVE_RESPONSE)` —
    /// the disconnect also counts while a `KeepAlive` for the session is
    /// outstanding.
    #[test]
    fn a_disconnect_ends_the_session_in_run_session_and_await_alive_response() {
        for state in ALL_STATES {
            let mut m = machine_in(state, false);
            m.session_client = Some(SESSION_CLIENT);
            let actions = m.handle(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT));
            if matches!(
                state,
                XdmcpState::RunSession | XdmcpState::AwaitAliveResponse
            ) {
                assert_eq!(
                    actions,
                    vec![XdmcpAction::ResetGeneration {
                        cause: RenewCause::SessionEnded
                    }],
                    "{state:?}"
                );
                assert_eq!(m.state(), XdmcpState::Query);
            } else {
                assert!(actions.is_empty(), "{state:?} ended a session");
                assert_eq!(m.state(), state);
            }
        }
    }

    /// `XdmcpCloseDisplay` neither cancels the timer nor sends: the re-query
    /// comes later, from `XdmcpReset` on the new generation.
    #[test]
    fn session_end_does_not_send_a_query_itself() {
        let mut m = at_run_session();
        let actions = m.handle(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT));
        assert_eq!(actions.len(), 1);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, XdmcpAction::Send { .. }))
        );

        // …and the new generation's Start is what re-queries.
        let restart = m.handle(XdmcpEvent::Start);
        assert!(restart.contains(&XdmcpAction::Send {
            destination: PacketDestination::Manager,
            message: XdmcpMessage::Query {
                authentication_names: vec![]
            },
        }));
    }

    /// The serialisation the design asks for, in both orders: a `Refuse` and
    /// an authenticated setup racing must never leave a running session whose
    /// cookie has just been cleared.
    #[test]
    fn a_refuse_and_a_setup_cannot_both_win() {
        // Refuse first: the setup is then a no-op.
        let mut m = at_await_manage_response();
        let refused = m.handle(packet(XdmcpMessage::Refuse {
            session_id: SESSION,
        }));
        assert!(refused.contains(&XdmcpAction::ClearCookie));
        assert_eq!(
            m.handle(XdmcpEvent::SessionClientEstablished(SESSION_CLIENT)),
            vec![]
        );
        assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);
        assert_eq!(m.session_client(), None);

        // Setup first: the Refuse is then a no-op and nothing is cleared.
        let mut m = at_await_manage_response();
        let _ = m.handle(XdmcpEvent::SessionClientEstablished(SESSION_CLIENT));
        assert_eq!(
            m.handle(packet(XdmcpMessage::Refuse {
                session_id: SESSION
            })),
            vec![]
        );
        assert_eq!(m.state(), XdmcpState::RunSession);
    }

    // -----------------------------------------------------------------
    // KeepAlive and Alive
    // -----------------------------------------------------------------

    /// `XdmcpTimerNotify` (`xdmcp.c:664`): in `RUN_SESSION` the timer means
    /// the dormancy elapsed, so send a `KeepAlive`.
    #[test]
    fn the_dormancy_timer_sends_a_keepalive() {
        let mut m = at_run_session();
        assert_eq!(
            m.handle(XdmcpEvent::TimerExpired),
            vec![
                XdmcpAction::Send {
                    destination: PacketDestination::SelectedHost(MANAGER),
                    message: XdmcpMessage::KeepAlive {
                        display_number: 7,
                        session_id: SESSION,
                    },
                },
                XdmcpAction::SetTimer { seconds: 2 },
            ]
        );
        assert_eq!(m.state(), XdmcpState::AwaitAliveResponse);
    }

    #[test]
    fn an_alive_saying_running_returns_to_the_session_and_rearms_dormancy() {
        let mut m = at_await_alive_response();
        assert_eq!(
            m.handle(packet(XdmcpMessage::Alive {
                session_running: 1,
                session_id: SESSION,
            })),
            vec![XdmcpAction::SetTimer {
                seconds: XDM_DEF_DORMANCY
            }]
        );
        assert_eq!(m.state(), XdmcpState::RunSession);
    }

    /// `if (SessionRunning && …)` is a truthiness test, so any non-zero byte
    /// counts as running.
    #[test]
    fn any_non_zero_session_running_byte_counts_as_running() {
        let mut m = at_await_alive_response();
        let _ = m.handle(packet(XdmcpMessage::Alive {
            session_running: 0xff,
            session_id: SESSION,
        }));
        assert_eq!(m.state(), XdmcpState::RunSession);
    }

    /// The `else` arm: `XdmcpDeadSession("Alive response indicates session
    /// dead")`. Both a zero `SessionRunning` and a foreign session id land
    /// here — note the difference from `Refuse`/`Failed`, where a foreign
    /// session id is merely ignored.
    #[test]
    fn an_alive_saying_dead_or_naming_another_session_kills_the_session() {
        for message in [
            XdmcpMessage::Alive {
                session_running: 0,
                session_id: SESSION,
            },
            XdmcpMessage::Alive {
                session_running: 1,
                session_id: SESSION ^ 1,
            },
        ] {
            let mut m = at_await_alive_response();
            let actions = m.handle(packet(message));
            assert_eq!(actions[0], XdmcpAction::CancelTimer);
            assert_eq!(
                actions[1],
                XdmcpAction::ResetGeneration {
                    cause: RenewCause::AliveSaysSessionDead
                }
            );
            assert_eq!(m.state(), XdmcpState::CollectQuery);
        }
    }

    /// `if (state != XDM_AWAIT_ALIVE_RESPONSE) return;` (`xdmcp.c:1322`).
    #[test]
    fn alive_is_ignored_outside_await_alive_response() {
        for state in ALL_STATES {
            if state == XdmcpState::AwaitAliveResponse {
                continue;
            }
            let mut m = machine_in(state, false);
            let actions = m.handle(packet(XdmcpMessage::Alive {
                session_running: 0,
                session_id: SESSION,
            }));
            assert!(actions.is_empty(), "{state:?} acted on Alive: {actions:?}");
            assert_eq!(m.state(), state);
        }
    }

    // -----------------------------------------------------------------
    // Retransmission
    // -----------------------------------------------------------------

    /// `timeout` (`xdmcp.c:819`) walks each awaiting state back to its
    /// sending state and resends.
    #[test]
    fn a_timeout_walks_each_awaiting_state_back_to_its_sending_state() {
        for (awaiting, resent) in [
            (XdmcpState::CollectQuery, XdmcpState::CollectQuery),
            (
                XdmcpState::CollectBroadcastQuery,
                XdmcpState::CollectBroadcastQuery,
            ),
            (
                XdmcpState::CollectIndirectQuery,
                XdmcpState::CollectIndirectQuery,
            ),
            (
                XdmcpState::AwaitRequestResponse,
                XdmcpState::AwaitRequestResponse,
            ),
            (
                XdmcpState::AwaitManageResponse,
                XdmcpState::AwaitManageResponse,
            ),
            (
                XdmcpState::AwaitAliveResponse,
                XdmcpState::AwaitAliveResponse,
            ),
        ] {
            let mut m = machine_in(awaiting, false);
            let actions = m.handle(XdmcpEvent::TimerExpired);
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, XdmcpAction::Send { .. })),
                "{awaiting:?} did not resend"
            );
            assert_eq!(m.state(), resent);
            assert_eq!(m.timeout_rtx(), 1);
        }
    }

    /// The backoff, walked all the way to the limit. `XDM_RTX_LIMIT` is 7, so
    /// the seventh expiry declares the session dead.
    #[test]
    fn seven_unanswered_retransmissions_reset_the_generation() {
        let mut m = machine(InitialMode::Query, false);
        let _ = m.handle(XdmcpEvent::Start);
        for (attempt, expected) in [(1u32, 4u32), (2, 8), (3, 16), (4, 32), (5, 32), (6, 32)] {
            let actions = m.handle(XdmcpEvent::TimerExpired);
            assert_eq!(m.timeout_rtx(), attempt);
            assert!(actions.contains(&XdmcpAction::SetTimer { seconds: expected }));
            assert_eq!(m.state(), XdmcpState::CollectQuery);
        }
        // The seventh expiry hits XDM_RTX_LIMIT.
        let actions = m.handle(XdmcpEvent::TimerExpired);
        assert_eq!(actions[0], XdmcpAction::CancelTimer);
        assert_eq!(
            actions[1],
            XdmcpAction::ResetGeneration {
                cause: RenewCause::RetransmissionsExhausted
            }
        );
        // XdmcpDeadSession resets the backoff and re-queries immediately.
        assert_eq!(m.timeout_rtx(), 0);
        assert_eq!(m.state(), XdmcpState::CollectQuery);
    }

    /// While awaiting `Alive` the lower `XDM_KA_RTX_LIMIT` of 4 applies, and
    /// it is checked *before* the generic limit.
    #[test]
    fn four_unanswered_keepalives_declare_the_session_dead() {
        let mut m = at_await_alive_response();
        for attempt in 1..=3u32 {
            let actions = m.handle(XdmcpEvent::TimerExpired);
            assert_eq!(m.timeout_rtx(), attempt);
            assert_eq!(m.state(), XdmcpState::AwaitAliveResponse);
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, XdmcpAction::Send { .. }))
            );
        }
        let actions = m.handle(XdmcpEvent::TimerExpired);
        assert_eq!(
            actions[1],
            XdmcpAction::ResetGeneration {
                cause: RenewCause::KeepAliveTimedOut
            }
        );
        assert_eq!(m.state(), XdmcpState::CollectQuery);
    }

    /// `receive_packet` sets `timeOutRtx = 0` at `xdmcp.c:728`, before it has
    /// read the header — so even a datagram we cannot decode cancels the
    /// backoff. Xorg-faithful, and worth knowing: it is remotely triggerable.
    #[test]
    fn any_datagram_including_an_undecodable_one_resets_the_backoff() {
        let mut m = machine(InitialMode::Query, false);
        let _ = m.handle(XdmcpEvent::Start);
        let _ = m.handle(XdmcpEvent::TimerExpired);
        let _ = m.handle(XdmcpEvent::TimerExpired);
        assert_eq!(m.timeout_rtx(), 2);
        assert_eq!(m.handle(XdmcpEvent::UndecodablePacket), vec![]);
        assert_eq!(m.timeout_rtx(), 0);

        let _ = m.handle(XdmcpEvent::TimerExpired);
        assert_eq!(m.timeout_rtx(), 1);
        // A decoded packet we ignore in this state does the same.
        let _ = m.handle(packet(XdmcpMessage::Alive {
            session_running: 1,
            session_id: SESSION,
        }));
        assert_eq!(m.timeout_rtx(), 0);
    }

    /// Reset targets the **configured** mode. `XDM_INIT_STATE` is a variable
    /// (`xdmcp.c:80`), not a state; hardcoding `Query` would work under
    /// `-query` and break `-broadcast` and `-indirect` from the second
    /// session on.
    #[test]
    fn every_reset_path_returns_to_the_configured_initial_mode() {
        for (mode, collecting) in [
            (InitialMode::Query, XdmcpState::CollectQuery),
            (InitialMode::Broadcast, XdmcpState::CollectBroadcastQuery),
            (InitialMode::Indirect, XdmcpState::CollectIndirectQuery),
        ] {
            // Session end: XdmcpCloseDisplay sets state and stops there.
            let mut m = machine(mode, false);
            m.state = XdmcpState::RunSession;
            m.session_client = Some(SESSION_CLIENT);
            let _ = m.handle(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT));
            assert_eq!(m.state(), mode.state(), "{mode:?} session end");

            // XdmcpDeadSession: sets state *and* re-queries in that mode.
            let mut m = machine_in(collecting, false);
            m.config.initial_mode = mode;
            for _ in 0..XDM_RTX_LIMIT {
                let _ = m.handle(XdmcpEvent::TimerExpired);
            }
            assert_eq!(m.state(), collecting, "{mode:?} dead session");
        }
    }

    // -----------------------------------------------------------------
    // -once
    // -----------------------------------------------------------------

    #[test]
    fn once_terminates_at_session_end_instead_of_resetting() {
        let mut m = machine(InitialMode::Query, true);
        m.state = XdmcpState::RunSession;
        m.session_client = Some(SESSION_CLIENT);
        assert_eq!(
            m.handle(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT)),
            vec![XdmcpAction::Terminate(TerminateReason::OneSession {
                cause: RenewCause::SessionEnded
            })]
        );
        assert_eq!(m.state(), XdmcpState::Off);
    }

    /// The case a happy-path suite skips: `-once` with a manager that never
    /// answers must **exit**, not loop. `timeout` checks `OneSession` before
    /// `XdmcpDeadSession` is ever reached (`xdmcp.c:826-834`), and that
    /// branch neither cancels the timer nor sends.
    #[test]
    fn once_terminates_on_retransmission_exhaustion_with_no_session_ever_established() {
        let mut m = machine(InitialMode::Query, true);
        let _ = m.handle(XdmcpEvent::Start);
        for _ in 0..(XDM_RTX_LIMIT - 1) {
            let actions = m.handle(XdmcpEvent::TimerExpired);
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, XdmcpAction::Send { .. }))
            );
        }
        assert_eq!(
            m.handle(XdmcpEvent::TimerExpired),
            vec![XdmcpAction::Terminate(TerminateReason::OneSession {
                cause: RenewCause::RetransmissionsExhausted
            })]
        );
        assert_eq!(m.state(), XdmcpState::Off);
    }

    #[test]
    fn once_terminates_on_keepalive_failure() {
        let mut m = machine_in(XdmcpState::AwaitAliveResponse, true);
        for _ in 0..(XDM_KA_RTX_LIMIT - 1) {
            let _ = m.handle(XdmcpEvent::TimerExpired);
        }
        assert_eq!(
            m.handle(XdmcpEvent::TimerExpired),
            vec![
                XdmcpAction::CancelTimer,
                XdmcpAction::Terminate(TerminateReason::OneSession {
                    cause: RenewCause::KeepAliveTimedOut
                })
            ]
        );
        assert_eq!(m.state(), XdmcpState::Off);
    }

    #[test]
    fn once_terminates_when_alive_says_the_session_is_dead() {
        let mut m = machine_in(XdmcpState::AwaitAliveResponse, true);
        assert_eq!(
            m.handle(packet(XdmcpMessage::Alive {
                session_running: 0,
                session_id: SESSION,
            })),
            vec![
                XdmcpAction::CancelTimer,
                XdmcpAction::Terminate(TerminateReason::OneSession {
                    cause: RenewCause::AliveSaysSessionDead
                })
            ]
        );
        assert_eq!(m.state(), XdmcpState::Off);
    }

    /// Under `-once` no renew path may emit a `ResetGeneration`, and none may
    /// send another query on the way out.
    #[test]
    fn no_once_renew_path_resets_the_generation_or_re_queries() {
        let paths = [
            (
                XdmcpState::RunSession,
                XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT),
            ),
            (XdmcpState::AwaitAliveResponse, XdmcpEvent::TimerExpired),
            (
                XdmcpState::AwaitAliveResponse,
                packet(XdmcpMessage::Alive {
                    session_running: 0,
                    session_id: SESSION,
                }),
            ),
        ];
        for (start, event) in paths {
            let mut m = machine_in(start, true);
            m.timeout_rtx = XDM_KA_RTX_LIMIT - 1;
            let actions = m.handle(event);
            assert!(
                !actions
                    .iter()
                    .any(|a| matches!(a, XdmcpAction::ResetGeneration { .. })),
                "{start:?} reset a generation under -once: {actions:?}"
            );
            assert!(
                !actions
                    .iter()
                    .any(|a| matches!(a, XdmcpAction::Send { .. })),
                "{start:?} re-queried under -once: {actions:?}"
            );
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, XdmcpAction::Terminate(_))),
                "{start:?} did not terminate under -once: {actions:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Off, and the exhaustive sweep
    // -----------------------------------------------------------------

    /// A terminated machine is inert (divergence 4): every event is a no-op.
    #[test]
    fn off_absorbs_every_event() {
        for event in all_events() {
            let mut m = machine_in(XdmcpState::Off, false);
            assert_eq!(m.handle(event.clone()), vec![], "Off acted on {event:?}");
            assert_eq!(m.state(), XdmcpState::Off);
        }
    }

    fn all_events() -> Vec<XdmcpEvent> {
        let messages = [
            XdmcpMessage::Query {
                authentication_names: vec![],
            },
            XdmcpMessage::BroadcastQuery {
                authentication_names: vec![],
            },
            XdmcpMessage::IndirectQuery {
                authentication_names: vec![],
            },
            willing(),
            XdmcpMessage::Unwilling {
                hostname: b"xdm".to_vec(),
                status: b"no".to_vec(),
            },
            XdmcpMessage::Request {
                display_number: 7,
                connection_types: vec![],
                connection_addresses: vec![],
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_names: vec![],
                manufacturer_display_id: vec![],
            },
            good_accept(),
            XdmcpMessage::Decline {
                status: b"no".to_vec(),
                authentication_name: vec![],
                authentication_data: vec![],
            },
            XdmcpMessage::Manage {
                session_id: SESSION,
                display_number: 7,
                display_class: vec![],
            },
            XdmcpMessage::Refuse {
                session_id: SESSION,
            },
            XdmcpMessage::Failed {
                session_id: SESSION,
                status: b"no".to_vec(),
            },
            XdmcpMessage::KeepAlive {
                display_number: 7,
                session_id: SESSION,
            },
            XdmcpMessage::Alive {
                session_running: 1,
                session_id: SESSION,
            },
        ];
        assert_eq!(messages.len(), 13);
        let mut events: Vec<XdmcpEvent> = messages.into_iter().map(packet).collect();
        events.push(XdmcpEvent::Start);
        events.push(XdmcpEvent::UndecodablePacket);
        events.push(XdmcpEvent::TimerExpired);
        events.push(XdmcpEvent::SessionClientEstablished(SESSION_CLIENT));
        events.push(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT));
        events
    }

    /// The display's own outgoing packets have no case in `receive_packet`'s
    /// switch (`xdmcp.c:736-756`), so one arriving is dropped in every state.
    #[test]
    fn our_own_outgoing_packet_types_are_ignored_wherever_they_arrive() {
        let ours = [
            XdmcpMessage::Query {
                authentication_names: vec![],
            },
            XdmcpMessage::BroadcastQuery {
                authentication_names: vec![],
            },
            XdmcpMessage::IndirectQuery {
                authentication_names: vec![],
            },
            XdmcpMessage::Request {
                display_number: 7,
                connection_types: vec![],
                connection_addresses: vec![],
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_names: vec![],
                manufacturer_display_id: vec![],
            },
            XdmcpMessage::Manage {
                session_id: SESSION,
                display_number: 7,
                display_class: vec![],
            },
            XdmcpMessage::KeepAlive {
                display_number: 7,
                session_id: SESSION,
            },
        ];
        for state in ALL_STATES {
            for message in &ours {
                let mut m = machine_in(state, false);
                let actions = m.handle(packet(message.clone()));
                assert!(
                    actions.is_empty(),
                    "{state:?} acted on {message:?}: {actions:?}"
                );
                assert_eq!(m.state(), state);
            }
        }
    }

    /// Every event in every state, for both `-once` settings: nothing panics,
    /// no transition lands outside the enum, and no action list contradicts
    /// itself by both terminating and resetting.
    #[test]
    fn every_event_in_every_state_is_defined() {
        for once in [false, true] {
            for state in ALL_STATES {
                for event in all_events() {
                    let mut m = machine_in(state, once);
                    let actions = m.handle(event.clone());
                    assert!(
                        ALL_STATES.contains(&m.state()),
                        "{state:?} + {event:?} left the enum"
                    );
                    let terminates = actions
                        .iter()
                        .any(|a| matches!(a, XdmcpAction::Terminate(_)));
                    let resets = actions
                        .iter()
                        .any(|a| matches!(a, XdmcpAction::ResetGeneration { .. }));
                    assert!(
                        !(terminates && resets),
                        "{state:?} + {event:?} both terminated and reset"
                    );
                    if terminates {
                        assert_eq!(
                            m.state(),
                            XdmcpState::Off,
                            "{state:?} + {event:?} terminated but stayed live"
                        );
                        assert!(
                            !actions
                                .iter()
                                .any(|a| matches!(a, XdmcpAction::Send { .. })),
                            "{state:?} + {event:?} sent a packet on the way out"
                        );
                    }
                    if once {
                        assert!(!resets, "{state:?} + {event:?} reset under -once");
                    }
                    // A state that sends must always arm a timer with it.
                    if actions
                        .iter()
                        .any(|a| matches!(a, XdmcpAction::Send { .. }))
                    {
                        assert!(
                            actions
                                .iter()
                                .any(|a| matches!(a, XdmcpAction::SetTimer { .. })),
                            "{state:?} + {event:?} sent without arming a timer"
                        );
                    }
                }
            }
        }
    }

    /// The whole happy path in one place, so the ordering of the actions is
    /// pinned end to end.
    #[test]
    fn the_happy_path_runs_query_request_manage_session_reset() {
        let mut m = machine(InitialMode::Query, false);

        assert!(m.handle(XdmcpEvent::Start).contains(&XdmcpAction::Send {
            destination: PacketDestination::Manager,
            message: XdmcpMessage::Query {
                authentication_names: vec![]
            },
        }));
        assert_eq!(m.state(), XdmcpState::CollectQuery);

        assert!(m.handle(packet(willing())).iter().any(|a| matches!(
            a,
            XdmcpAction::Send {
                message: XdmcpMessage::Request { .. },
                ..
            }
        )));
        assert_eq!(m.state(), XdmcpState::AwaitRequestResponse);

        let accepted = m.handle(packet(good_accept()));
        assert_eq!(
            accepted[0],
            XdmcpAction::InstallCookie {
                name: MIT_MAGIC_COOKIE_1.to_vec(),
                data: COOKIE.to_vec(),
            }
        );
        assert_eq!(m.state(), XdmcpState::AwaitManageResponse);

        assert_eq!(
            m.handle(XdmcpEvent::SessionClientEstablished(SESSION_CLIENT)),
            vec![XdmcpAction::SetTimer {
                seconds: XDM_DEF_DORMANCY
            }]
        );
        assert_eq!(m.state(), XdmcpState::RunSession);

        assert_eq!(
            m.handle(XdmcpEvent::SessionClientDisconnected(SESSION_CLIENT)),
            vec![XdmcpAction::ResetGeneration {
                cause: RenewCause::SessionEnded
            }]
        );
        assert_eq!(m.state(), XdmcpState::Query);

        // The second session starts on the same machine.
        let _ = m.handle(XdmcpEvent::Start);
        assert_eq!(m.state(), XdmcpState::CollectQuery);
    }
}
