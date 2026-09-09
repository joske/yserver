//! XDMCP wire codec.
//!
//! Every field layout here is taken from the XDMCP specification's "Protocol
//! Encoding" chapter (`/usr/share/doc/libXdmcp/xdmcp.xml`), cross-checked
//! against the senders and receivers in `/home/jos/Projects/xserver/os/xdmcp.c`.
//!
//! Two rules from the spec's "Data Types" chapter govern everything below:
//!
//! > Integer values are always stored most significant byte first in the
//! > packet ("Big Endian" order). […] no padding of any sort will occur
//! > within the packets.
//!
//! The primitives, with their sizes from the same chapter:
//!
//! | Type | Layout |
//! |---|---|
//! | `CARD8` / `CARD16` / `CARD32` | 1 / 2 / 4 bytes, big-endian |
//! | `ARRAY8` | `CARD16` count `n`, then `n` × `CARD8` |
//! | `ARRAY16` | `CARD8` count `m`, then `m` × `CARD16` |
//! | `ARRAY32` | `CARD8` count `l`, then `l` × `CARD32` |
//! | `ARRAYofARRAY8` | `CARD8` count, then that many `ARRAY8` |
//!
//! Note the asymmetry, which is a classic source of wrong codecs: `ARRAY8`
//! counts *bytes* in a `CARD16`, while the other three count *elements* in a
//! `CARD8`.
//!
//! Every packet begins with the 6-byte header (`XdmcpHeader`,
//! `X11/Xdmcp.h:100`): `CARD16` version, `CARD16` opcode, `CARD16` length.
//! **`length` counts the body only**, excluding those six bytes — see the
//! per-packet formulas in the spec (e.g. Willing's `length (6 + m + n + o)`)
//! and the arithmetic in `xdmcp.c`'s receivers.

/// `XDM_PROTOCOL_VERSION` (`X11/Xdmcp.h:25`). `receive_packet` drops any
/// packet whose version differs (`xdmcp.c:733`).
pub const XDM_PROTOCOL_VERSION: u16 = 1;

/// `XDM_UDP_PORT` (`X11/Xdmcp.h:26`).
pub const XDM_UDP_PORT: u16 = 177;

/// `XDM_MAX_MSGLEN` (`X11/Xdmcp.h:36`) — the size of Xorg's `XdmcpBuffer`.
pub const XDM_MAX_MSGLEN: usize = 8192;

/// Bytes in the `XdmcpHeader` that precede the body the `length` field counts.
pub const HEADER_LEN: usize = 6;

// Opcodes, from the `xdmOpCode` enum (`X11/Xdmcp.h:46-50`) and the encoding
// table in the spec's "Protocol Encoding" chapter. The spec carries a
// footnote that an earlier revision of the document reversed KeepAlive and
// Alive; 13/14 as below is the corrected assignment and is what `xdmcp.c`
// uses.
/// `BROADCAST_QUERY = 1`.
pub const BROADCAST_QUERY: u16 = 1;
/// `QUERY = 2`.
pub const QUERY: u16 = 2;
/// `INDIRECT_QUERY = 3`.
pub const INDIRECT_QUERY: u16 = 3;
/// `FORWARD_QUERY = 4` — recognised as a valid opcode but not implemented;
/// it is a manager-to-manager packet and `xdmcp.c`'s `receive_packet` has no
/// case for it either.
pub const FORWARD_QUERY: u16 = 4;
/// `WILLING = 5`.
pub const WILLING: u16 = 5;
/// `UNWILLING = 6`.
pub const UNWILLING: u16 = 6;
/// `REQUEST = 7`.
pub const REQUEST: u16 = 7;
/// `ACCEPT = 8`.
pub const ACCEPT: u16 = 8;
/// `DECLINE = 9`.
pub const DECLINE: u16 = 9;
/// `MANAGE = 10`.
pub const MANAGE: u16 = 10;
/// `REFUSE = 11`.
pub const REFUSE: u16 = 11;
/// `FAILED = 12`.
pub const FAILED: u16 = 12;
/// `KEEPALIVE = 13`.
pub const KEEPALIVE: u16 = 13;
/// `ALIVE = 14`.
pub const ALIVE: u16 = 14;

/// A decoded XDMCP packet.
///
/// The thirteen types the display side of the protocol uses. `ForwardQuery`
/// is deliberately absent: it travels manager-to-manager and `xdmcp.c` never
/// sends or receives it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XdmcpMessage {
    /// Query, opcode 2. `send_query_msg`, `xdmcp.c:954`.
    Query {
        /// `AuthenticationNames`, an `ARRAYofARRAY8`.
        authentication_names: Vec<Vec<u8>>,
    },
    /// BroadcastQuery, opcode 1 — identical body to `Query`.
    BroadcastQuery { authentication_names: Vec<Vec<u8>> },
    /// IndirectQuery, opcode 3 — identical body to `Query`.
    IndirectQuery { authentication_names: Vec<Vec<u8>> },
    /// Willing, opcode 5. `recv_willing_msg`, `xdmcp.c:1044`.
    Willing {
        authentication_name: Vec<u8>,
        hostname: Vec<u8>,
        status: Vec<u8>,
    },
    /// Unwilling, opcode 6.
    ///
    /// `receive_packet` (`xdmcp.c:740`) never parses this body — it goes
    /// straight to `XdmcpFatal` with a canned "Host unwilling" status — so
    /// the layout here comes from the spec's encoding chapter alone.
    Unwilling { hostname: Vec<u8>, status: Vec<u8> },
    /// Request, opcode 7. `send_request_msg`, `xdmcp.c:1081`.
    Request {
        display_number: u16,
        /// `ConnectionTypes`, an `ARRAY16`.
        connection_types: Vec<u16>,
        /// `ConnectionAddresses`, an `ARRAYofARRAY8`.
        connection_addresses: Vec<Vec<u8>>,
        authentication_name: Vec<u8>,
        authentication_data: Vec<u8>,
        /// `AuthorizationNames`, an `ARRAYofARRAY8`.
        authorization_names: Vec<Vec<u8>>,
        manufacturer_display_id: Vec<u8>,
    },
    /// Accept, opcode 8. `recv_accept_msg`, `xdmcp.c:1168`.
    Accept {
        session_id: u32,
        authentication_name: Vec<u8>,
        authentication_data: Vec<u8>,
        authorization_name: Vec<u8>,
        authorization_data: Vec<u8>,
    },
    /// Decline, opcode 9. `recv_decline_msg`, `xdmcp.c:1213`.
    Decline {
        status: Vec<u8>,
        authentication_name: Vec<u8>,
        authentication_data: Vec<u8>,
    },
    /// Manage, opcode 10. `send_manage_msg`, `xdmcp.c:1237`.
    Manage {
        session_id: u32,
        display_number: u16,
        display_class: Vec<u8>,
    },
    /// Refuse, opcode 11. `recv_refuse_msg`, `xdmcp.c:1260`.
    Refuse { session_id: u32 },
    /// Failed, opcode 12. `recv_failed_msg`, `xdmcp.c:1277`.
    Failed { session_id: u32, status: Vec<u8> },
    /// KeepAlive, opcode 13. `send_keepalive_msg`, `xdmcp.c:1295`.
    KeepAlive {
        display_number: u16,
        session_id: u32,
    },
    /// Alive, opcode 14. `recv_alive_msg`, `xdmcp.c:1317`.
    Alive {
        /// `Session Running` is a `CARD8`, and `recv_alive_msg` tests it for
        /// mere truthiness rather than equality with 1. Kept as the raw byte
        /// so a round-trip is lossless; use [`XdmcpMessage::alive_running`]
        /// for the truthiness test Xorg applies.
        session_running: u8,
        session_id: u32,
    },
}

impl XdmcpMessage {
    /// The opcode this message encodes with.
    #[must_use]
    pub fn opcode(&self) -> u16 {
        match self {
            Self::Query { .. } => QUERY,
            Self::BroadcastQuery { .. } => BROADCAST_QUERY,
            Self::IndirectQuery { .. } => INDIRECT_QUERY,
            Self::Willing { .. } => WILLING,
            Self::Unwilling { .. } => UNWILLING,
            Self::Request { .. } => REQUEST,
            Self::Accept { .. } => ACCEPT,
            Self::Decline { .. } => DECLINE,
            Self::Manage { .. } => MANAGE,
            Self::Refuse { .. } => REFUSE,
            Self::Failed { .. } => FAILED,
            Self::KeepAlive { .. } => KEEPALIVE,
            Self::Alive { .. } => ALIVE,
        }
    }

    /// `SessionRunning` as `recv_alive_msg` (`xdmcp.c:1328`) tests it: any
    /// non-zero byte means the manager says the session is running.
    #[must_use]
    pub fn alive_running(&self) -> bool {
        matches!(self, Self::Alive { session_running, .. } if *session_running != 0)
    }
}

/// Why a packet could not be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// The datagram ran out before a field was complete. Xorg's equivalent is
    /// an `XdmcpRead*` returning FALSE because `buffer->pointer` would pass
    /// `buffer->count`, after which the receiver silently drops the packet.
    Truncated,
    /// `header.version != XDM_PROTOCOL_VERSION` (`xdmcp.c:733`).
    UnsupportedVersion(u16),
    /// An opcode with no case in `receive_packet`'s switch, or outside the
    /// `xdmOpCode` enum entirely.
    UnknownOpcode(u16),
    /// The header's declared `length` does not equal the number of body bytes
    /// the fields actually occupy.
    ///
    /// This is the check every receiver in `xdmcp.c` performs by hand —
    /// `recv_accept_msg`'s `length == 12 + …` (`:1185`), `recv_refuse_msg`'s
    /// `length != 4` (`:1256`), and so on. Without it a decoder happily
    /// accepts a packet whose declared length is a lie.
    LengthMismatch {
        /// The header's `length` field.
        declared: u16,
        /// Body bytes the fields consumed.
        actual: usize,
    },
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "XDMCP packet truncated"),
            Self::UnsupportedVersion(v) => write!(f, "XDMCP protocol version {v} unsupported"),
            Self::UnknownOpcode(op) => write!(f, "XDMCP opcode {op} unknown"),
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "XDMCP declared length {declared} but fields occupy {actual} bytes"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why a message could not be encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// An `ARRAY8` longer than its `CARD16` count field can express.
    ArrayTooLong(usize),
    /// An `ARRAY16`, `ARRAY32` or `ARRAYofARRAY8` with more than 255 elements
    /// — their counts are `CARD8`. `XdmcpRegisterConnection` guards the same
    /// limit at `xdmcp.c:500` (`if (ConnectionAddresses.length + 1 == 256)`).
    TooManyElements(usize),
    /// A body that does not fit the header's `CARD16` length field, or a
    /// packet past `XDM_MAX_MSGLEN`.
    MessageTooLong(usize),
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ArrayTooLong(n) => write!(f, "XDMCP ARRAY8 of {n} bytes exceeds CARD16"),
            Self::TooManyElements(n) => write!(f, "XDMCP array of {n} elements exceeds CARD8"),
            Self::MessageTooLong(n) => write!(f, "XDMCP packet of {n} bytes too long"),
        }
    }
}

impl std::error::Error for EncodeError {}

// ---------------------------------------------------------------------------
// Writer primitives
// ---------------------------------------------------------------------------

fn write_card8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn write_card16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_card32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// `ARRAY8`: `CARD16` byte count, then the bytes. `XdmcpWriteARRAY8`.
fn write_array8(out: &mut Vec<u8>, data: &[u8]) -> Result<(), EncodeError> {
    let len = u16::try_from(data.len()).map_err(|_| EncodeError::ArrayTooLong(data.len()))?;
    write_card16(out, len);
    out.extend_from_slice(data);
    Ok(())
}

/// `ARRAY16`: `CARD8` element count, then that many big-endian `CARD16`.
/// `XdmcpWriteARRAY16`.
fn write_array16(out: &mut Vec<u8>, values: &[u16]) -> Result<(), EncodeError> {
    let len = u8::try_from(values.len()).map_err(|_| EncodeError::TooManyElements(values.len()))?;
    write_card8(out, len);
    for value in values {
        write_card16(out, *value);
    }
    Ok(())
}

/// `ARRAY32`: `CARD8` element count, then that many big-endian `CARD32`.
/// `XdmcpWriteARRAY32`.
///
/// None of the thirteen display-side packets carries an `ARRAY32`; it is part
/// of the protocol's primitive set (spec "Data Types") and of libXdmcp's API,
/// so it is provided and tested here for completeness.
pub fn write_array32(out: &mut Vec<u8>, values: &[u32]) -> Result<(), EncodeError> {
    let len = u8::try_from(values.len()).map_err(|_| EncodeError::TooManyElements(values.len()))?;
    write_card8(out, len);
    for value in values {
        write_card32(out, *value);
    }
    Ok(())
}

/// `ARRAYofARRAY8`: `CARD8` element count, then that many `ARRAY8`.
/// `XdmcpWriteARRAYofARRAY8`.
fn write_array_of_array8(out: &mut Vec<u8>, arrays: &[Vec<u8>]) -> Result<(), EncodeError> {
    let len = u8::try_from(arrays.len()).map_err(|_| EncodeError::TooManyElements(arrays.len()))?;
    write_card8(out, len);
    for array in arrays {
        write_array8(out, array)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reader primitives
// ---------------------------------------------------------------------------

/// A bounds-checked cursor over a packet body.
///
/// Xorg's `XdmcpBuffer` reads are bounded by `count` — the bytes actually
/// received — not by the header's declared length, so a body longer than the
/// header claims still parses and the mismatch is caught afterwards. This
/// mirrors that.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn card8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn card16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn card32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// `XdmcpReadARRAY8`.
    fn array8(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.card16()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    /// `XdmcpReadARRAY16`.
    fn array16(&mut self) -> Result<Vec<u16>, DecodeError> {
        let len = self.card8()? as usize;
        let mut values = Vec::with_capacity(len);
        for _ in 0..len {
            values.push(self.card16()?);
        }
        Ok(values)
    }

    /// `XdmcpReadARRAY32`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no display-side packet carries an ARRAY32")
    )]
    fn array32(&mut self) -> Result<Vec<u32>, DecodeError> {
        let len = self.card8()? as usize;
        let mut values = Vec::with_capacity(len);
        for _ in 0..len {
            values.push(self.card32()?);
        }
        Ok(values)
    }

    /// `XdmcpReadARRAYofARRAY8`.
    fn array_of_array8(&mut self) -> Result<Vec<Vec<u8>>, DecodeError> {
        let len = self.card8()? as usize;
        let mut arrays = Vec::with_capacity(len);
        for _ in 0..len {
            arrays.push(self.array8()?);
        }
        Ok(arrays)
    }
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Encode a message, header included.
///
/// The header's `length` is the encoded body size, which is exactly the
/// per-packet formula the spec's encoding chapter gives — `12 + n + m + o + p`
/// for Accept, `8 + m` for Manage, and so on — so it is computed rather than
/// restated.
///
/// # Errors
///
/// [`EncodeError`] if an array exceeds its count field or the packet exceeds
/// [`XDM_MAX_MSGLEN`].
pub fn encode_message(message: &XdmcpMessage) -> Result<Vec<u8>, EncodeError> {
    let mut body = Vec::new();
    match message {
        XdmcpMessage::Query {
            authentication_names,
        }
        | XdmcpMessage::BroadcastQuery {
            authentication_names,
        }
        | XdmcpMessage::IndirectQuery {
            authentication_names,
        } => {
            write_array_of_array8(&mut body, authentication_names)?;
        }
        XdmcpMessage::Willing {
            authentication_name,
            hostname,
            status,
        } => {
            write_array8(&mut body, authentication_name)?;
            write_array8(&mut body, hostname)?;
            write_array8(&mut body, status)?;
        }
        XdmcpMessage::Unwilling { hostname, status } => {
            write_array8(&mut body, hostname)?;
            write_array8(&mut body, status)?;
        }
        XdmcpMessage::Request {
            display_number,
            connection_types,
            connection_addresses,
            authentication_name,
            authentication_data,
            authorization_names,
            manufacturer_display_id,
        } => {
            write_card16(&mut body, *display_number);
            write_array16(&mut body, connection_types)?;
            write_array_of_array8(&mut body, connection_addresses)?;
            write_array8(&mut body, authentication_name)?;
            write_array8(&mut body, authentication_data)?;
            write_array_of_array8(&mut body, authorization_names)?;
            write_array8(&mut body, manufacturer_display_id)?;
        }
        XdmcpMessage::Accept {
            session_id,
            authentication_name,
            authentication_data,
            authorization_name,
            authorization_data,
        } => {
            write_card32(&mut body, *session_id);
            write_array8(&mut body, authentication_name)?;
            write_array8(&mut body, authentication_data)?;
            write_array8(&mut body, authorization_name)?;
            write_array8(&mut body, authorization_data)?;
        }
        XdmcpMessage::Decline {
            status,
            authentication_name,
            authentication_data,
        } => {
            write_array8(&mut body, status)?;
            write_array8(&mut body, authentication_name)?;
            write_array8(&mut body, authentication_data)?;
        }
        XdmcpMessage::Manage {
            session_id,
            display_number,
            display_class,
        } => {
            write_card32(&mut body, *session_id);
            write_card16(&mut body, *display_number);
            write_array8(&mut body, display_class)?;
        }
        XdmcpMessage::Refuse { session_id } => write_card32(&mut body, *session_id),
        XdmcpMessage::Failed { session_id, status } => {
            write_card32(&mut body, *session_id);
            write_array8(&mut body, status)?;
        }
        XdmcpMessage::KeepAlive {
            display_number,
            session_id,
        } => {
            write_card16(&mut body, *display_number);
            write_card32(&mut body, *session_id);
        }
        XdmcpMessage::Alive {
            session_running,
            session_id,
        } => {
            write_card8(&mut body, *session_running);
            write_card32(&mut body, *session_id);
        }
    }

    let length = u16::try_from(body.len()).map_err(|_| EncodeError::MessageTooLong(body.len()))?;
    let total = HEADER_LEN + body.len();
    if total > XDM_MAX_MSGLEN {
        return Err(EncodeError::MessageTooLong(total));
    }

    let mut out = Vec::with_capacity(total);
    write_card16(&mut out, XDM_PROTOCOL_VERSION);
    write_card16(&mut out, message.opcode());
    write_card16(&mut out, length);
    out.extend_from_slice(&body);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// Decode a datagram.
///
/// Validation is in three layers, matching `xdmcp.c`:
///
/// 1. `XdmcpReadHeader` plus the version test at `:733`.
/// 2. Field reads bounded by the bytes actually present — Xorg's
///    `XdmcpRead*` returning FALSE.
/// 3. The declared-length arithmetic every receiver performs by hand. A
///    decoder that skips this accepts a truncated packet whose fields happen
///    to parse, which is why it is enforced here for all thirteen types
///    rather than only for `Accept`.
///
/// Bytes past the declared length are ignored, as they are by Xorg: its
/// receivers stop reading once their fields are consumed and only compare
/// `length` against the fields, never against `buffer->count`.
///
/// # Errors
///
/// [`DecodeError`] if the packet is truncated, carries an unsupported
/// version, an unhandled opcode, or a declared length that disagrees with its
/// fields.
pub fn decode_message(packet: &[u8]) -> Result<XdmcpMessage, DecodeError> {
    // `XdmcpReadHeader` (`xdmcp.c:730`) reads all three fields and only then
    // does `receive_packet` test the version, so a header shorter than six
    // bytes is a truncation rather than a version complaint.
    let mut header = Reader::new(packet);
    let version = header.card16()?;
    let opcode = header.card16()?;
    let declared = header.card16()?;
    if version != XDM_PROTOCOL_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }

    let body = &packet[HEADER_LEN..];
    let mut r = Reader::new(body);
    let message = match opcode {
        QUERY => XdmcpMessage::Query {
            authentication_names: r.array_of_array8()?,
        },
        BROADCAST_QUERY => XdmcpMessage::BroadcastQuery {
            authentication_names: r.array_of_array8()?,
        },
        INDIRECT_QUERY => XdmcpMessage::IndirectQuery {
            authentication_names: r.array_of_array8()?,
        },
        WILLING => XdmcpMessage::Willing {
            authentication_name: r.array8()?,
            hostname: r.array8()?,
            status: r.array8()?,
        },
        UNWILLING => XdmcpMessage::Unwilling {
            hostname: r.array8()?,
            status: r.array8()?,
        },
        REQUEST => XdmcpMessage::Request {
            display_number: r.card16()?,
            connection_types: r.array16()?,
            connection_addresses: r.array_of_array8()?,
            authentication_name: r.array8()?,
            authentication_data: r.array8()?,
            authorization_names: r.array_of_array8()?,
            manufacturer_display_id: r.array8()?,
        },
        ACCEPT => XdmcpMessage::Accept {
            session_id: r.card32()?,
            authentication_name: r.array8()?,
            authentication_data: r.array8()?,
            authorization_name: r.array8()?,
            authorization_data: r.array8()?,
        },
        DECLINE => XdmcpMessage::Decline {
            status: r.array8()?,
            authentication_name: r.array8()?,
            authentication_data: r.array8()?,
        },
        MANAGE => XdmcpMessage::Manage {
            session_id: r.card32()?,
            display_number: r.card16()?,
            display_class: r.array8()?,
        },
        REFUSE => XdmcpMessage::Refuse {
            session_id: r.card32()?,
        },
        FAILED => XdmcpMessage::Failed {
            session_id: r.card32()?,
            status: r.array8()?,
        },
        KEEPALIVE => XdmcpMessage::KeepAlive {
            display_number: r.card16()?,
            session_id: r.card32()?,
        },
        ALIVE => XdmcpMessage::Alive {
            session_running: r.card8()?,
            session_id: r.card32()?,
        },
        other => return Err(DecodeError::UnknownOpcode(other)),
    };

    if usize::from(declared) != r.pos {
        return Err(DecodeError::LengthMismatch {
            declared,
            actual: r.pos,
        });
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Byte vectors.
    //
    // Every vector below is written out field by field from the spec's
    // "Protocol Encoding" chapter (`/usr/share/doc/libXdmcp/xdmcp.xml`,
    // reproduced in the comment above each one) and its declared length
    // computed by hand from the formula the spec gives for that packet.
    // None of them was produced by `encode_message`; that is the point —
    // a self-consistent codec that is wrong on the wire passes every
    // round-trip test.
    // -----------------------------------------------------------------

    /// Assert both directions against a vector nothing in this crate produced.
    fn check(vector: &[u8], message: &XdmcpMessage) {
        assert_eq!(
            decode_message(vector).as_ref(),
            Ok(message),
            "decode disagrees with the spec vector"
        );
        assert_eq!(
            encode_message(message).as_deref(),
            Ok(vector),
            "encode disagrees with the spec vector"
        );
    }

    // 2 CARD16 version 1 / 2 CARD16 opcode Query / 2 CARD16 length /
    // 1 CARD8 number of Authentication Names (m=0)
    //
    // length = 1 (the count byte alone), matching `send_query_msg`'s
    // `header.length = 1;` before its per-name loop (`xdmcp.c:990`).
    #[test]
    fn query_with_no_authentication_names_matches_the_spec_vector() {
        let vector = [0x00, 0x01, 0x00, 0x02, 0x00, 0x01, 0x00];
        check(
            &vector,
            &XdmcpMessage::Query {
                authentication_names: vec![],
            },
        );
    }

    // Same layout with one name, exercising the ARRAYofARRAY8 element
    // encoding: count 1, then ARRAY8{CARD16 20, "XDM-AUTHENTICATION-1"}.
    // length = 1 + (2 + 20) = 23 = 0x17.
    #[test]
    fn query_with_one_authentication_name_matches_the_spec_vector() {
        let mut vector = vec![0x00, 0x01, 0x00, 0x02, 0x00, 0x17, 0x01, 0x00, 0x14];
        vector.extend_from_slice(b"XDM-AUTHENTICATION-1");
        assert_eq!(b"XDM-AUTHENTICATION-1".len(), 0x14);
        check(
            &vector,
            &XdmcpMessage::Query {
                authentication_names: vec![b"XDM-AUTHENTICATION-1".to_vec()],
            },
        );
    }

    // "Note that these three packets are identical except for the opcode
    // field." — spec, Protocol Encoding. BroadcastQuery is opcode 1.
    #[test]
    fn broadcast_query_matches_the_spec_vector() {
        let vector = [0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00];
        check(
            &vector,
            &XdmcpMessage::BroadcastQuery {
                authentication_names: vec![],
            },
        );
    }

    // IndirectQuery is opcode 3.
    #[test]
    fn indirect_query_matches_the_spec_vector() {
        let vector = [0x00, 0x01, 0x00, 0x03, 0x00, 0x01, 0x00];
        check(
            &vector,
            &XdmcpMessage::IndirectQuery {
                authentication_names: vec![],
            },
        );
    }

    // Willing: length (6 + m + n + o); ARRAY8 Authentication Name (m=0),
    // ARRAY8 Hostname (n=3, "xdm"), ARRAY8 Status (o=17).
    // length = 6 + 0 + 3 + 17 = 26 = 0x1a.
    #[test]
    fn willing_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x05, // opcode Willing
            0x00, 0x1a, // length 26
            0x00, 0x00, // Authentication Name, length 0
            0x00, 0x03, // Hostname, length 3
        ];
        vector.extend_from_slice(b"xdm");
        vector.extend_from_slice(&[0x00, 0x11]); // Status, length 17
        vector.extend_from_slice(b"Willing to manage");
        assert_eq!(b"Willing to manage".len(), 0x11);
        check(
            &vector,
            &XdmcpMessage::Willing {
                authentication_name: vec![],
                hostname: b"xdm".to_vec(),
                status: b"Willing to manage".to_vec(),
            },
        );
    }

    // Unwilling: length (4 + m + n); ARRAY8 Hostname (m=3), ARRAY8 Status
    // (n=14 — "Host unwilling", the exact `UnwillingMessage` literal at
    // `xdmcp.c:710`). length = 4 + 3 + 14 = 21 = 0x15.
    #[test]
    fn unwilling_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x06, // opcode Unwilling
            0x00, 0x15, // length 21
            0x00, 0x03, // Hostname, length 3
        ];
        vector.extend_from_slice(b"xdm");
        vector.extend_from_slice(&[0x00, 0x0e]); // Status, length 14
        vector.extend_from_slice(b"Host unwilling");
        assert_eq!(b"Host unwilling".len(), 0x0e);
        check(
            &vector,
            &XdmcpMessage::Unwilling {
                hostname: b"xdm".to_vec(),
                status: b"Host unwilling".to_vec(),
            },
        );
    }

    // Request: CARD16 Display Number, ARRAY16 Connection Types,
    // ARRAYofARRAY8 Connection Addresses, ARRAY8 Authentication Name,
    // ARRAY8 Authentication Data, ARRAYofARRAY8 Authorization Names,
    // ARRAY8 Manufacturer Display ID.
    //
    // Connection type 0 is FamilyInternet (`X11/X.h`), which is what
    // `send_request_msg` writes for an AF_INET manager (`xdmcp.c:1092`).
    //
    //   2                    display number             ->  2
    //   1 + 2*1              ARRAY16 of one type        ->  5
    //   1 + (2 + 4)          one 4-byte IPv4 address    -> 12
    //   2 + 0                empty authentication name  -> 14
    //   2 + 0                empty authentication data  -> 16
    //   1 + (2 + 18)         one authorization name     -> 37
    //   2 + 0                empty display ID           -> 39 = 0x27
    #[test]
    fn request_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x07, // opcode Request
            0x00, 0x27, // length 39
            0x00, 0x07, // Display Number 7
            0x01, // Count of Connection Types
            0x00, 0x00, // FamilyInternet
            0x01, // Count of Connection Addresses
            0x00, 0x04, // first address, length 4
            0xc0, 0xa8, 0x01, 0x05, // 192.168.1.5
            0x00, 0x00, // Authentication Name, length 0
            0x00, 0x00, // Authentication Data, length 0
            0x01, // Count of Authorization Names
            0x00, 0x12, // first name, length 18
        ];
        vector.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        vector.extend_from_slice(&[0x00, 0x00]); // Manufacturer Display ID
        assert_eq!(b"MIT-MAGIC-COOKIE-1".len(), 0x12);
        check(
            &vector,
            &XdmcpMessage::Request {
                display_number: 7,
                connection_types: vec![0],
                connection_addresses: vec![vec![192, 168, 1, 5]],
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_names: vec![b"MIT-MAGIC-COOKIE-1".to_vec()],
                manufacturer_display_id: vec![],
            },
        );
    }

    /// The 16 bytes an `mcookie`-style cookie occupies. Not a protocol
    /// constant — the spec imposes no length on the authorization data.
    const COOKIE: [u8; 16] = [
        0xde, 0xad, 0xbe, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xed, 0xfa,
        0xce,
    ];

    // Accept: length (12 + n + m + o + p); CARD32 Session ID, then four
    // ARRAY8. 12 = 4 (session id) + 4 * 2 (the four CARD16 counts), which
    // is exactly the arithmetic `recv_accept_msg` checks at `xdmcp.c:1185`.
    // length = 12 + 0 + 0 + 18 + 16 = 46 = 0x2e.
    #[test]
    fn accept_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x08, // opcode Accept
            0x00, 0x2e, // length 46
            0x12, 0x34, 0x56, 0x78, // Session ID
            0x00, 0x00, // Authentication Name, length 0
            0x00, 0x00, // Authentication Data, length 0
            0x00, 0x12, // Authorization Name, length 18
        ];
        vector.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        vector.extend_from_slice(&[0x00, 0x10]); // Authorization Data, length 16
        vector.extend_from_slice(&COOKIE);
        check(
            &vector,
            &XdmcpMessage::Accept {
                session_id: 0x1234_5678,
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_name: b"MIT-MAGIC-COOKIE-1".to_vec(),
                authorization_data: COOKIE.to_vec(),
            },
        );
    }

    // Decline: length (6 + m + n + o); ARRAY8 Status, ARRAY8 Authentication
    // Name, ARRAY8 Authentication Data. Note Status comes *first*, unlike
    // Failed where the Session ID leads.
    // length = 6 + 13 + 0 + 0 = 19 = 0x13.
    #[test]
    fn decline_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x09, // opcode Decline
            0x00, 0x13, // length 19
            0x00, 0x0d, // Status, length 13
        ];
        vector.extend_from_slice(b"No permission");
        vector.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // empty name, empty data
        assert_eq!(b"No permission".len(), 0x0d);
        check(
            &vector,
            &XdmcpMessage::Decline {
                status: b"No permission".to_vec(),
                authentication_name: vec![],
                authentication_data: vec![],
            },
        );
    }

    // Manage: length (8 + m); CARD32 Session ID, CARD16 Display Number,
    // ARRAY8 Display Class. 8 = 4 + 2 + 2, matching `send_manage_msg`'s
    // `header.length = 8 + DisplayClass.length;` (`xdmcp.c:1244`).
    // "MIT-unspecified" is `defaultDisplayClass` (`xdmcp.c:65`), 15 bytes.
    // length = 8 + 15 = 23 = 0x17.
    #[test]
    fn manage_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x0a, // opcode Manage
            0x00, 0x17, // length 23
            0x12, 0x34, 0x56, 0x78, // Session ID
            0x00, 0x07, // Display Number 7
            0x00, 0x0f, // Display Class, length 15
        ];
        vector.extend_from_slice(b"MIT-unspecified");
        assert_eq!(b"MIT-unspecified".len(), 0x0f);
        check(
            &vector,
            &XdmcpMessage::Manage {
                session_id: 0x1234_5678,
                display_number: 7,
                display_class: b"MIT-unspecified".to_vec(),
            },
        );
    }

    // Refuse: length (4); CARD32 Session ID. The literal `length != 4`
    // guard at `xdmcp.c:1266`.
    #[test]
    fn refuse_matches_the_spec_vector() {
        let vector = [
            0x00, 0x01, // version 1
            0x00, 0x0b, // opcode Refuse
            0x00, 0x04, // length 4
            0x12, 0x34, 0x56, 0x78, // Session ID
        ];
        check(
            &vector,
            &XdmcpMessage::Refuse {
                session_id: 0x1234_5678,
            },
        );
    }

    // Failed: length (6 + m); CARD32 Session ID, ARRAY8 Status.
    // length = 6 + 14 = 20 = 0x14.
    #[test]
    fn failed_matches_the_spec_vector() {
        let mut vector = vec![
            0x00, 0x01, // version 1
            0x00, 0x0c, // opcode Failed
            0x00, 0x14, // length 20
            0x12, 0x34, 0x56, 0x78, // Session ID
            0x00, 0x0e, // Status, length 14
        ];
        vector.extend_from_slice(b"Session failed");
        assert_eq!(b"Session failed".len(), 0x0e);
        check(
            &vector,
            &XdmcpMessage::Failed {
                session_id: 0x1234_5678,
                status: b"Session failed".to_vec(),
            },
        );
    }

    // KeepAlive: length (6); CARD16 Display Number, CARD32 Session ID.
    // Display Number leads — the opposite order to Manage, and the reason
    // `send_keepalive_msg` writes CARD16 before CARD32 (`xdmcp.c:1295`).
    #[test]
    fn keepalive_matches_the_spec_vector() {
        let vector = [
            0x00, 0x01, // version 1
            0x00, 0x0d, // opcode KeepAlive
            0x00, 0x06, // length 6
            0x00, 0x07, // Display Number 7
            0x12, 0x34, 0x56, 0x78, // Session ID
        ];
        check(
            &vector,
            &XdmcpMessage::KeepAlive {
                display_number: 7,
                session_id: 0x1234_5678,
            },
        );
    }

    // Alive: length (5); CARD8 Session Running, CARD32 Session ID.
    // The odd 5-byte body is why `recv_alive_msg` tests `length != 5`
    // (`xdmcp.c:1324`).
    #[test]
    fn alive_matches_the_spec_vector() {
        let vector = [
            0x00, 0x01, // version 1
            0x00, 0x0e, // opcode Alive
            0x00, 0x05, // length 5
            0x01, // Session Running
            0x12, 0x34, 0x56, 0x78, // Session ID
        ];
        check(
            &vector,
            &XdmcpMessage::Alive {
                session_running: 1,
                session_id: 0x1234_5678,
            },
        );
    }

    // Spec: "Session Running (0: not running 1: running)" and
    // "Session ID (0: not running)".
    #[test]
    fn alive_not_running_matches_the_spec_vector() {
        let vector = [
            0x00, 0x01, 0x00, 0x0e, 0x00, 0x05, 0x00, // Session Running = 0
            0x00, 0x00, 0x00, 0x00, // Session ID = 0
        ];
        let message = XdmcpMessage::Alive {
            session_running: 0,
            session_id: 0,
        };
        check(&vector, &message);
        assert!(!message.alive_running());
    }

    // `recv_alive_msg` tests `if (SessionRunning && …)`, so any non-zero
    // byte counts as running.
    #[test]
    fn alive_running_is_truthiness_not_equality_with_one() {
        let message = XdmcpMessage::Alive {
            session_running: 0x42,
            session_id: 1,
        };
        assert!(message.alive_running());
        let encoded = encode_message(&message).expect("encodes");
        assert_eq!(encoded[6], 0x42);
        assert_eq!(decode_message(&encoded), Ok(message));
    }

    // -----------------------------------------------------------------
    // Cross-cutting wire facts
    // -----------------------------------------------------------------

    /// The opcode table from the spec's Protocol Encoding chapter, which is
    /// also the `xdmOpCode` enum at `X11/Xdmcp.h:46-50`. The spec footnotes
    /// that an earlier revision reversed KeepAlive and Alive; these are the
    /// corrected values.
    #[test]
    fn opcodes_match_the_protocol_table() {
        assert_eq!(BROADCAST_QUERY, 1);
        assert_eq!(QUERY, 2);
        assert_eq!(INDIRECT_QUERY, 3);
        assert_eq!(FORWARD_QUERY, 4);
        assert_eq!(WILLING, 5);
        assert_eq!(UNWILLING, 6);
        assert_eq!(REQUEST, 7);
        assert_eq!(ACCEPT, 8);
        assert_eq!(DECLINE, 9);
        assert_eq!(MANAGE, 10);
        assert_eq!(REFUSE, 11);
        assert_eq!(FAILED, 12);
        assert_eq!(KEEPALIVE, 13);
        assert_eq!(ALIVE, 14);
        assert_eq!(XDM_PROTOCOL_VERSION, 1);
        assert_eq!(XDM_UDP_PORT, 177);
        assert_eq!(XDM_MAX_MSGLEN, 8192);
    }

    /// "Integer values are always stored most significant byte first in the
    /// packet" — spec, Data Types. A little-endian codec passes every
    /// round-trip test and talks to nothing.
    #[test]
    fn integers_are_big_endian_on_the_wire() {
        let encoded = encode_message(&XdmcpMessage::KeepAlive {
            display_number: 0x0102,
            session_id: 0x0a0b_0c0d,
        })
        .expect("encodes");
        assert_eq!(
            encoded,
            vec![
                0x00, 0x01, // version, MSB first
                0x00, 0x0d, // opcode, MSB first
                0x00, 0x06, // length, MSB first
                0x01, 0x02, // display number, MSB first
                0x0a, 0x0b, 0x0c, 0x0d, // session id, MSB first
            ]
        );
    }

    /// "no padding of any sort will occur within the packets" — spec, Data
    /// Types. Alive's 1-byte `SessionRunning` is followed immediately by a
    /// CARD32 at an odd offset, which a padding-inserting codec would align.
    #[test]
    fn no_padding_is_inserted_between_fields() {
        let encoded = encode_message(&XdmcpMessage::Alive {
            session_running: 1,
            session_id: 0xaabb_ccdd,
        })
        .expect("encodes");
        assert_eq!(encoded.len(), HEADER_LEN + 5);
        assert_eq!(&encoded[6..], &[0x01, 0xaa, 0xbb, 0xcc, 0xdd]);
    }

    /// `ARRAY8` counts bytes in a `CARD16`; `ARRAY16`, `ARRAY32` and
    /// `ARRAYofARRAY8` count *elements* in a `CARD8`. Getting this backwards
    /// is the primitive-level mistake round-trips cannot catch.
    #[test]
    fn array_count_fields_have_the_widths_the_spec_gives() {
        let mut out = Vec::new();
        write_array8(&mut out, b"ab").expect("fits");
        assert_eq!(out, vec![0x00, 0x02, b'a', b'b']);

        let mut out = Vec::new();
        write_array16(&mut out, &[0x0102, 0x0304]).expect("fits");
        assert_eq!(out, vec![0x02, 0x01, 0x02, 0x03, 0x04]);

        let mut out = Vec::new();
        write_array32(&mut out, &[0x0102_0304]).expect("fits");
        assert_eq!(out, vec![0x01, 0x01, 0x02, 0x03, 0x04]);

        let mut out = Vec::new();
        write_array_of_array8(&mut out, &[b"a".to_vec(), b"bc".to_vec()]).expect("fits");
        assert_eq!(out, vec![0x02, 0x00, 0x01, b'a', 0x00, 0x02, b'b', b'c']);
    }

    /// The `ARRAY32` reader/writer pair, which no display-side packet uses
    /// but the protocol's primitive set defines (spec, Data Types:
    /// "a CARD8 (l) which specifies the number of CARD32 values to follow").
    #[test]
    fn array32_round_trips_through_the_primitive_pair() {
        let mut out = Vec::new();
        write_array32(&mut out, &[1, 0xdead_beef, 0]).expect("fits");
        assert_eq!(out[0], 3);
        assert_eq!(out.len(), 1 + 3 * 4);
        let mut r = Reader::new(&out);
        assert_eq!(r.array32(), Ok(vec![1, 0xdead_beef, 0]));
    }

    #[test]
    fn an_array8_longer_than_card16_is_refused() {
        let mut out = Vec::new();
        let huge = vec![0u8; usize::from(u16::MAX) + 1];
        assert_eq!(
            write_array8(&mut out, &huge),
            Err(EncodeError::ArrayTooLong(65536))
        );
    }

    #[test]
    fn more_than_255_elements_are_refused() {
        let mut out = Vec::new();
        let arrays = vec![Vec::new(); 256];
        assert_eq!(
            write_array_of_array8(&mut out, &arrays),
            Err(EncodeError::TooManyElements(256))
        );
        let mut out = Vec::new();
        assert_eq!(
            write_array16(&mut out, &vec![0u16; 256]),
            Err(EncodeError::TooManyElements(256))
        );
    }

    #[test]
    fn a_packet_past_xdm_max_msglen_is_refused() {
        let message = XdmcpMessage::Failed {
            session_id: 1,
            status: vec![b'x'; XDM_MAX_MSGLEN],
        };
        assert_eq!(
            encode_message(&message),
            Err(EncodeError::MessageTooLong(HEADER_LEN + 6 + XDM_MAX_MSGLEN))
        );
    }

    // -----------------------------------------------------------------
    // Declared-length arithmetic — the trap the plan names explicitly
    // -----------------------------------------------------------------

    /// The prescribed case: an `Accept` whose header still claims 46 bytes
    /// but whose 16-byte cookie is cut to 4. A decoder that ignores the
    /// declared length hands the state machine a 4-byte cookie and calls it
    /// a session credential.
    #[test]
    fn a_truncated_accept_is_refused_not_silently_shortened() {
        let mut vector = vec![
            0x00, 0x01, 0x00, 0x08, 0x00, 0x2e, // length still claims 46
            0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12,
        ];
        vector.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        vector.extend_from_slice(&[0x00, 0x10]); // says 16 bytes of cookie
        vector.extend_from_slice(&COOKIE[..4]); // …but only 4 are present
        assert_eq!(decode_message(&vector), Err(DecodeError::Truncated));
    }

    /// The other half: every field is present and parses, but the header's
    /// length disagrees. `recv_accept_msg`'s `length == 12 + …` test
    /// (`xdmcp.c:1185`) is what rejects this, and it is the only thing that
    /// does — the field reads all succeed.
    #[test]
    fn an_accept_whose_declared_length_disagrees_with_its_fields_is_refused() {
        let mut vector = vec![
            0x00, 0x01, 0x00, 0x08, 0x00, 0x2d, // 45, one short of the real 46
            0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12,
        ];
        vector.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
        vector.extend_from_slice(&[0x00, 0x10]);
        vector.extend_from_slice(&COOKIE);
        assert_eq!(
            decode_message(&vector),
            Err(DecodeError::LengthMismatch {
                declared: 0x2d,
                actual: 0x2e,
            })
        );
    }

    /// Every one of the thirteen enforces its own length formula, not just
    /// `Accept`. Each vector below is a good packet with its declared length
    /// bumped by one.
    #[test]
    fn every_message_type_enforces_its_declared_length() {
        let messages = [
            XdmcpMessage::Query {
                authentication_names: vec![b"a".to_vec()],
            },
            XdmcpMessage::BroadcastQuery {
                authentication_names: vec![],
            },
            XdmcpMessage::IndirectQuery {
                authentication_names: vec![],
            },
            XdmcpMessage::Willing {
                authentication_name: vec![],
                hostname: b"xdm".to_vec(),
                status: b"ok".to_vec(),
            },
            XdmcpMessage::Unwilling {
                hostname: b"xdm".to_vec(),
                status: b"no".to_vec(),
            },
            XdmcpMessage::Request {
                display_number: 0,
                connection_types: vec![0],
                connection_addresses: vec![vec![127, 0, 0, 1]],
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_names: vec![b"MIT-MAGIC-COOKIE-1".to_vec()],
                manufacturer_display_id: vec![],
            },
            XdmcpMessage::Accept {
                session_id: 1,
                authentication_name: vec![],
                authentication_data: vec![],
                authorization_name: b"MIT-MAGIC-COOKIE-1".to_vec(),
                authorization_data: COOKIE.to_vec(),
            },
            XdmcpMessage::Decline {
                status: b"no".to_vec(),
                authentication_name: vec![],
                authentication_data: vec![],
            },
            XdmcpMessage::Manage {
                session_id: 1,
                display_number: 0,
                display_class: b"MIT-unspecified".to_vec(),
            },
            XdmcpMessage::Refuse { session_id: 1 },
            XdmcpMessage::Failed {
                session_id: 1,
                status: b"no".to_vec(),
            },
            XdmcpMessage::KeepAlive {
                display_number: 0,
                session_id: 1,
            },
            XdmcpMessage::Alive {
                session_running: 1,
                session_id: 1,
            },
        ];
        assert_eq!(messages.len(), 13);
        for message in &messages {
            let good = encode_message(message).expect("encodes");
            assert_eq!(decode_message(&good).as_ref(), Ok(message));

            let body_len = good.len() - HEADER_LEN;
            let mut bad = good.clone();
            let inflated = u16::try_from(body_len).expect("small") + 1;
            bad[4..6].copy_from_slice(&inflated.to_be_bytes());
            // Pad so the extra byte is genuinely present: this isolates the
            // length check from the truncation check.
            bad.push(0);
            assert_eq!(
                decode_message(&bad),
                Err(DecodeError::LengthMismatch {
                    declared: inflated,
                    actual: body_len,
                }),
                "{message:?} accepted an inflated declared length"
            );
        }
    }

    /// Bytes beyond the declared length are ignored, as they are by Xorg:
    /// its receivers read only their own fields and compare `length` against
    /// those, never against the datagram size.
    #[test]
    fn trailing_bytes_past_the_declared_length_are_ignored() {
        let mut vector = vec![0x00, 0x01, 0x00, 0x0b, 0x00, 0x04, 0x12, 0x34, 0x56, 0x78];
        vector.extend_from_slice(b"trailing garbage");
        assert_eq!(
            decode_message(&vector),
            Ok(XdmcpMessage::Refuse {
                session_id: 0x1234_5678
            })
        );
    }

    // -----------------------------------------------------------------
    // Header rejection
    // -----------------------------------------------------------------

    #[test]
    fn a_short_header_is_truncated_not_a_panic() {
        for len in 0..HEADER_LEN {
            let bytes = vec![0x00; len];
            assert_eq!(decode_message(&bytes), Err(DecodeError::Truncated));
        }
    }

    /// `receive_packet` drops anything that is not version 1
    /// (`xdmcp.c:733`).
    #[test]
    fn a_foreign_protocol_version_is_refused() {
        let vector = [0x00, 0x02, 0x00, 0x02, 0x00, 0x01, 0x00];
        assert_eq!(
            decode_message(&vector),
            Err(DecodeError::UnsupportedVersion(2))
        );
    }

    /// `ForwardQuery` is a real opcode but travels manager-to-manager, and
    /// `receive_packet`'s switch has no case for it. Opcode 0 and 15 are
    /// outside the enum.
    #[test]
    fn opcodes_we_do_not_handle_are_refused() {
        for opcode in [0u16, FORWARD_QUERY, 15, 0xffff] {
            let mut vector = vec![0x00, 0x01];
            vector.extend_from_slice(&opcode.to_be_bytes());
            vector.extend_from_slice(&[0x00, 0x00]);
            assert_eq!(
                decode_message(&vector),
                Err(DecodeError::UnknownOpcode(opcode))
            );
        }
    }

    /// A `Refuse` with a body of the wrong size — Xorg's `length != 4`.
    #[test]
    fn a_refuse_of_the_wrong_length_is_refused() {
        let vector = [
            0x00, 0x01, 0x00, 0x0b, 0x00, 0x05, // length 5, not 4
            0x12, 0x34, 0x56, 0x78, 0x00,
        ];
        assert_eq!(
            decode_message(&vector),
            Err(DecodeError::LengthMismatch {
                declared: 5,
                actual: 4
            })
        );
    }

    /// An `Alive` with a body of the wrong size — Xorg's `length != 5`.
    #[test]
    fn an_alive_of_the_wrong_length_is_refused() {
        let vector = [
            0x00, 0x01, 0x00, 0x0e, 0x00, 0x04, // length 4, not 5
            0x01, 0x12, 0x34, 0x56, 0x78,
        ];
        assert_eq!(
            decode_message(&vector),
            Err(DecodeError::LengthMismatch {
                declared: 4,
                actual: 5
            })
        );
    }

    /// An `ARRAY8` count that runs past the datagram. This is the shape of
    /// the first untrusted input in the whole flow.
    #[test]
    fn an_array8_count_past_the_end_of_the_datagram_is_refused() {
        let vector = [
            0x00, 0x01, 0x00, 0x05, 0x00, 0x1a, // Willing, length 26
            0x00, 0x00, // empty authentication name
            0xff, 0xff, // hostname claims 65535 bytes
        ];
        assert_eq!(decode_message(&vector), Err(DecodeError::Truncated));
    }

    /// An `ARRAYofARRAY8` element count that runs past the datagram.
    #[test]
    fn an_array_of_array8_count_past_the_end_of_the_datagram_is_refused() {
        let vector = [
            0x00, 0x01, 0x00, 0x02, 0x00, 0x01, // Query, length 1
            0xff, // claims 255 authentication names
        ];
        assert_eq!(decode_message(&vector), Err(DecodeError::Truncated));
    }

    // -----------------------------------------------------------------
    // Round-trips (necessary, not sufficient — the vectors above are what
    // pin the wire format)
    // -----------------------------------------------------------------

    #[test]
    fn empty_and_maximal_arrays_round_trip() {
        let message = XdmcpMessage::Request {
            display_number: u16::MAX,
            connection_types: vec![0; 255],
            connection_addresses: vec![vec![0xab; 4]; 255],
            authentication_name: vec![],
            authentication_data: vec![0xcd; 1024],
            authorization_names: vec![],
            manufacturer_display_id: b"-Ethernet-8:0:2b:a:f:d2".to_vec(),
        };
        let encoded = encode_message(&message).expect("encodes");
        assert_eq!(decode_message(&encoded), Ok(message));
    }
}
