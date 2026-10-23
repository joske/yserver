//! A minimal little-endian X client connection: requests out, replies,
//! errors and events in. Enough for the clipboard bridge (and its tests).

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

/// How long a reply or an awaited event may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const PROPERTY_NOTIFY: u8 = 28;
pub(crate) const SELECTION_CLEAR: u8 = 29;
pub(crate) const SELECTION_REQUEST: u8 = 30;
pub(crate) const SELECTION_NOTIFY: u8 = 31;
const GENERIC_EVENT: u8 = 35;

pub(crate) const ATOM: u32 = 4;
pub(crate) const INTEGER: u32 = 19;
pub(crate) const STRING: u32 = 31;

/// `PropertyChangeMask`.
pub(crate) const PROPERTY_CHANGE_MASK: u32 = 0x0040_0000;
/// `CWEventMask`.
const CW_EVENT_MASK: u32 = 0x0800;

/// A 32-byte event.
pub(crate) type Event = [u8; 32];

/// The `u32` at byte `at`.
pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn pad4(n: usize) -> usize {
    n.next_multiple_of(4)
}

/// An X error reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct XError {
    pub(crate) code: u8,
    pub(crate) major: u8,
}

/// A `GetProperty` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Property {
    /// 0: the property does not exist.
    pub(crate) type_: u32,
    pub(crate) format: u8,
    pub(crate) bytes_after: u32,
    pub(crate) data: Vec<u8>,
}

/// A request builder: opcode, data byte, then 4-byte-aligned fields; the
/// length is filled in by `finish`.
struct Req(Vec<u8>);

impl Req {
    fn new(opcode: u8, data: u8) -> Self {
        Self(vec![opcode, data, 0, 0])
    }

    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self.0.resize(pad4(self.0.len()), 0);
        self
    }

    fn finish(mut self) -> Vec<u8> {
        let units = u16::try_from(self.0.len() / 4).unwrap_or(0);
        self.0[2..4].copy_from_slice(&units.to_le_bytes());
        self.0
    }
}

pub(crate) struct Conn {
    stream: UnixStream,
    /// Bytes read but not yet parsed into packets.
    input: Vec<u8>,
    /// The last request's sequence number.
    seq: u16,
    events: VecDeque<Event>,
    pub(crate) root: u32,
    id_base: u32,
    id_mask: u32,
    next_id: u32,
    /// The largest request, in bytes.
    pub(crate) max_request_bytes: usize,
}

impl Conn {
    /// Run the connection setup on `stream` (no authorization).
    pub(crate) fn setup(mut stream: UnixStream) -> io::Result<Self> {
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])?;
        let mut header = [0; 8];
        stream.read_exact(&mut header)?;
        let mut body = vec![0; usize::from(u16_at(&header, 6)) * 4];
        stream.read_exact(&mut body)?;
        if header[0] != 1 {
            let reason = body.get(..usize::from(header[1])).unwrap_or(&body);
            return Err(io::Error::other(format!(
                "setup refused: {}",
                String::from_utf8_lossy(reason)
            )));
        }
        if body.len() < 40 {
            return Err(io::Error::other("short setup reply"));
        }
        let vendor = usize::from(u16_at(&body, 16));
        let formats = usize::from(body[21]);
        let screen = 32 + pad4(vendor) + 8 * formats;
        if body.len() < screen + 4 {
            return Err(io::Error::other("setup reply without a screen"));
        }
        Ok(Self {
            stream,
            input: Vec::new(),
            seq: 0,
            events: VecDeque::new(),
            root: u32_at(&body, screen),
            id_base: u32_at(&body, 4),
            id_mask: u32_at(&body, 8),
            next_id: 0,
            max_request_bytes: usize::from(u16_at(&body, 18)) * 4,
        })
    }

    /// A fresh resource id.
    pub(crate) fn new_id(&mut self) -> u32 {
        self.next_id += 1;
        self.id_base | (self.next_id & self.id_mask)
    }

    fn send(&mut self, request: &[u8]) -> io::Result<u16> {
        self.stream.write_all(request)?;
        self.seq = self.seq.wrapping_add(1);
        Ok(self.seq)
    }

    /// Read what arrives within `timeout` into the input buffer; false on a
    /// timeout.
    fn fill(&mut self, timeout: Duration) -> io::Result<bool> {
        self.stream
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut chunk = [0; 64 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                self.input.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// The next whole packet in the input buffer.
    fn packet(&mut self) -> Option<Vec<u8>> {
        if self.input.len() < 32 {
            return None;
        }
        let len = if self.input[0] == 1 || self.input[0] & 0x7f == GENERIC_EVENT {
            32 + usize::try_from(u32_at(&self.input, 4)).ok()? * 4
        } else {
            32
        };
        if self.input.len() < len {
            return None;
        }
        Some(self.input.drain(..len).collect())
    }

    /// Wait for the reply to request `seq`, queueing events.
    fn reply(&mut self, seq: u16) -> io::Result<Result<Vec<u8>, XError>> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            while let Some(packet) = self.packet() {
                let packet_seq = u16_at(&packet, 2);
                match packet[0] {
                    0 if packet_seq == seq => {
                        return Ok(Err(XError {
                            code: packet[1],
                            major: packet[10],
                        }));
                    }
                    0 => log_error(&packet),
                    1 if packet_seq == seq => return Ok(Ok(packet)),
                    1 => {}
                    _ => self.queue_event(&packet),
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply"));
            }
            self.fill(left)?;
        }
    }

    fn queue_event(&mut self, packet: &[u8]) {
        if let Ok(event) = Event::try_from(&packet[..32]) {
            self.events.push_back(event);
        }
    }

    /// The next event, waiting at most `timeout`. Errors to requests nobody
    /// waits for are logged and dropped.
    pub(crate) fn event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            while let Some(packet) = self.packet() {
                match packet[0] {
                    0 => log_error(&packet),
                    1 => {}
                    _ => self.queue_event(&packet),
                }
            }
            if !self.events.is_empty() {
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.fill(left)? {
                return Ok(self.events.pop_front());
            }
        }
    }

    /// Wait for the event `wanted` picks, keeping the others queued in order.
    pub(crate) fn wait_event(&mut self, wanted: impl Fn(&Event) -> bool) -> io::Result<Event> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut kept = VecDeque::new();
        let found = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.event(left)? {
                Some(event) if wanted(&event) => break Ok(event),
                Some(event) => kept.push_back(event),
                None if left.is_zero() => {
                    break Err(io::Error::new(io::ErrorKind::TimedOut, "no event"));
                }
                None => {}
            }
        };
        kept.append(&mut self.events);
        self.events = kept;
        found
    }

    /// `GetInputFocus` round trip: every earlier request has been handled.
    #[cfg(all(test, kms))]
    pub(crate) fn sync(&mut self) -> io::Result<()> {
        let seq = self.send(&Req::new(43, 0).finish())?;
        self.reply(seq)?
            .map(drop)
            .map_err(|e| io::Error::other(format!("GetInputFocus: {e:?}")))
    }

    pub(crate) fn intern_atom(&mut self, name: &str) -> io::Result<u32> {
        let len = u16::try_from(name.len()).map_err(io::Error::other)?;
        let request = Req::new(16, 0)
            .u16(len)
            .u16(0)
            .bytes(name.as_bytes())
            .finish();
        let seq = self.send(&request)?;
        match self.reply(seq)? {
            Ok(reply) => Ok(u32_at(&reply, 8)),
            Err(e) => Err(io::Error::other(format!("InternAtom {name}: {e:?}"))),
        }
    }

    /// `QueryExtension`: `(major opcode, first event)`, or `None`.
    pub(crate) fn query_extension(&mut self, name: &str) -> io::Result<Option<(u8, u8)>> {
        let len = u16::try_from(name.len()).map_err(io::Error::other)?;
        let request = Req::new(98, 0)
            .u16(len)
            .u16(0)
            .bytes(name.as_bytes())
            .finish();
        let seq = self.send(&request)?;
        match self.reply(seq)? {
            Ok(reply) if reply[8] != 0 => Ok(Some((reply[9], reply[10]))),
            Ok(_) => Ok(None),
            Err(e) => Err(io::Error::other(format!("QueryExtension {name}: {e:?}"))),
        }
    }

    /// `XFixesQueryVersion`, required before any other XFIXES request.
    pub(crate) fn xfixes_query_version(&mut self, major: u8) -> io::Result<()> {
        let seq = self.send(&Req::new(major, 0).u32(5).u32(0).finish())?;
        self.reply(seq)?
            .map(drop)
            .map_err(|e| io::Error::other(format!("XFixesQueryVersion: {e:?}")))
    }

    pub(crate) fn xfixes_select_selection_input(
        &mut self,
        major: u8,
        window: u32,
        selection: u32,
        mask: u32,
    ) -> io::Result<()> {
        let request = Req::new(major, 2)
            .u32(window)
            .u32(selection)
            .u32(mask)
            .finish();
        self.send(&request).map(drop)
    }

    /// An unmapped 1x1 `InputOutput` child of the root selecting
    /// `event_mask`.
    pub(crate) fn create_window(&mut self, event_mask: u32) -> io::Result<u32> {
        let window = self.new_id();
        let request = Req::new(1, 0)
            .u32(window)
            .u32(self.root)
            .u16(0xfff6) // x = -10
            .u16(0xfff6) // y = -10
            .u16(1)
            .u16(1)
            .u16(0) // border
            .u16(1) // InputOutput
            .u32(0) // CopyFromParent visual
            .u32(CW_EVENT_MASK)
            .u32(event_mask)
            .finish();
        self.send(&request)?;
        Ok(window)
    }

    /// Set this client's event mask on `window`.
    pub(crate) fn select_input(&mut self, window: u32, event_mask: u32) -> io::Result<()> {
        let request = Req::new(2, 0)
            .u32(window)
            .u32(CW_EVENT_MASK)
            .u32(event_mask)
            .finish();
        self.send(&request).map(drop)
    }

    /// `ChangeProperty` with `mode` 0 (Replace) or 2 (Append); `data` holds
    /// whole `format`-bit items.
    pub(crate) fn change_property(
        &mut self,
        mode: u8,
        window: u32,
        property: u32,
        type_: u32,
        format: u8,
        data: &[u8],
    ) -> io::Result<()> {
        let items = data.len() / usize::from(format / 8).max(1);
        let request = Req::new(18, mode)
            .u32(window)
            .u32(property)
            .u32(type_)
            .u32(u32::from(format))
            .u32(u32::try_from(items).map_err(io::Error::other)?)
            .bytes(data)
            .finish();
        self.send(&request).map(drop)
    }

    /// `GetProperty` of everything, any type; `delete` as the request's.
    pub(crate) fn get_property(
        &mut self,
        window: u32,
        property: u32,
        delete: bool,
    ) -> io::Result<Result<Property, XError>> {
        let request = Req::new(20, u8::from(delete))
            .u32(window)
            .u32(property)
            .u32(0) // AnyPropertyType
            .u32(0)
            .u32(0x1fff_ffff)
            .finish();
        let seq = self.send(&request)?;
        Ok(self.reply(seq)?.map(|reply| {
            let format = reply[1];
            let items = usize::try_from(u32_at(&reply, 16)).unwrap_or(0);
            let len = (items * usize::from(format / 8)).min(reply.len() - 32);
            Property {
                type_: u32_at(&reply, 8),
                format,
                bytes_after: u32_at(&reply, 12),
                data: reply[32..32 + len].to_vec(),
            }
        }))
    }

    pub(crate) fn delete_property(&mut self, window: u32, property: u32) -> io::Result<()> {
        let request = Req::new(19, 0).u32(window).u32(property).finish();
        self.send(&request).map(drop)
    }

    pub(crate) fn set_selection_owner(
        &mut self,
        owner: u32,
        selection: u32,
        time: u32,
    ) -> io::Result<()> {
        let request = Req::new(22, 0).u32(owner).u32(selection).u32(time).finish();
        self.send(&request).map(drop)
    }

    pub(crate) fn get_selection_owner(&mut self, selection: u32) -> io::Result<u32> {
        let seq = self.send(&Req::new(23, 0).u32(selection).finish())?;
        match self.reply(seq)? {
            Ok(reply) => Ok(u32_at(&reply, 8)),
            Err(e) => Err(io::Error::other(format!("GetSelectionOwner: {e:?}"))),
        }
    }

    pub(crate) fn convert_selection(
        &mut self,
        requestor: u32,
        selection: u32,
        target: u32,
        property: u32,
        time: u32,
    ) -> io::Result<()> {
        let request = Req::new(24, 0)
            .u32(requestor)
            .u32(selection)
            .u32(target)
            .u32(property)
            .u32(time)
            .finish();
        self.send(&request).map(drop)
    }

    /// `SendEvent` of a `SelectionNotify` to `requestor` (no propagation,
    /// empty mask: the window's owner gets it).
    pub(crate) fn send_selection_notify(
        &mut self,
        time: u32,
        requestor: u32,
        selection: u32,
        target: u32,
        property: u32,
    ) -> io::Result<()> {
        let mut event = [0_u8; 32];
        event[0] = SELECTION_NOTIFY;
        for (at, v) in [
            (4, time),
            (8, requestor),
            (12, selection),
            (16, target),
            (20, property),
        ] {
            event[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        let request = Req::new(25, 0).u32(requestor).u32(0).bytes(&event).finish();
        self.send(&request).map(drop)
    }

    /// `GrabServer`: other clients' requests wait until `ungrab_server`.
    #[cfg(all(test, kms))]
    pub(crate) fn grab_server(&mut self) -> io::Result<()> {
        self.send(&Req::new(36, 0).finish()).map(drop)
    }

    #[cfg(all(test, kms))]
    pub(crate) fn ungrab_server(&mut self) -> io::Result<()> {
        self.send(&Req::new(37, 0).finish()).map(drop)
    }

    /// `KillClient` of `resource`'s client.
    #[cfg(all(test, kms))]
    pub(crate) fn kill_client(&mut self, resource: u32) -> io::Result<()> {
        self.send(&Req::new(113, 0).u32(resource).finish())
            .map(drop)
    }
}

fn log_error(packet: &[u8]) {
    log::debug!(
        "clipboard: X error {} on request {} (seq {})",
        packet[1],
        packet[10],
        u16_at(packet, 2)
    );
}
