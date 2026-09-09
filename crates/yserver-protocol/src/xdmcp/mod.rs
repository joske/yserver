//! XDMCP — the X Display Manager Control Protocol.
//!
//! Specification: `/usr/share/doc/libXdmcp/xdmcp.xml` (shipped by libXdmcp;
//! its "Data Types", "Packet Format" and "Protocol Encoding" chapters are the
//! normative field layouts used here).
//! Constants: `/usr/include/X11/Xdmcp.h`.
//! Behaviour reference: `/home/jos/Projects/xserver/os/xdmcp.c`.
//!
//! Design: `docs/superpowers/specs/2026-09-09-xdmcp-design.md`.

pub mod codec;
pub mod state;

pub use codec::{
    ACCEPT, ALIVE, BROADCAST_QUERY, DECLINE, DecodeError, EncodeError, FAILED, FORWARD_QUERY,
    INDIRECT_QUERY, KEEPALIVE, MANAGE, QUERY, REFUSE, REQUEST, UNWILLING, WILLING, XDM_MAX_MSGLEN,
    XDM_PROTOCOL_VERSION, XDM_UDP_PORT, XdmcpMessage, decode_message, encode_message,
};
pub use state::{
    FatalKind, InitialMode, MIT_MAGIC_COOKIE_1, PacketDestination, RenewCause, TerminateReason,
    UNWILLING_MESSAGE, XDM_DEF_DORMANCY, XDM_KA_RTX_LIMIT, XDM_MAX_RTX, XDM_MIN_RTX, XDM_RTX_LIMIT,
    XdmcpAction, XdmcpConfig, XdmcpEvent, XdmcpMachine, XdmcpState, retransmit_seconds,
};
