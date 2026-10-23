//! The bridge's X side: watches CLIPBOARD with XFIXES, converts a new
//! owner's text for the host, and owns CLIPBOARD for host text (ICCCM §2,
//! INCR both ways). With the sync setting off it stands aside: no polling,
//! no conversions, no ownership.

use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use super::{
    Pasteboard,
    conn::{
        ATOM, Conn, Event, INTEGER, PROPERTY_CHANGE_MASK, PROPERTY_NOTIFY, Property,
        SELECTION_CLEAR, SELECTION_NOTIFY, SELECTION_REQUEST, STRING, u32_at,
    },
};

/// How often the host pasteboard is polled (it has no change notification).
pub(super) const POLL: Duration = Duration::from_millis(250);
/// An unanswered conversion or a stalled INCR transfer is dropped after this.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
/// XFIXES `SetSelectionOwner | SelectionWindowDestroy | SelectionClientClose`.
const XFIXES_SELECTION_MASK: u32 = 0b111;
/// `ChangeProperty` request header bytes.
const CHANGE_PROPERTY_HEADER: usize = 24;

struct Atoms {
    clipboard: u32,
    utf8_string: u32,
    text: u32,
    targets: u32,
    timestamp: u32,
    multiple: u32,
    atom_pair: u32,
    incr: u32,
    /// Our window's property a conversion lands in.
    transfer: u32,
    /// Our window's property touched to learn the server time.
    time: u32,
}

impl Atoms {
    fn intern(conn: &mut Conn) -> io::Result<Self> {
        Ok(Self {
            clipboard: conn.intern_atom("CLIPBOARD")?,
            utf8_string: conn.intern_atom("UTF8_STRING")?,
            text: conn.intern_atom("TEXT")?,
            targets: conn.intern_atom("TARGETS")?,
            timestamp: conn.intern_atom("TIMESTAMP")?,
            multiple: conn.intern_atom("MULTIPLE")?,
            atom_pair: conn.intern_atom("ATOM_PAIR")?,
            incr: conn.intern_atom("INCR")?,
            transfer: conn.intern_atom("YSERVER_CLIPBOARD")?,
            time: conn.intern_atom("YSERVER_TIME")?,
        })
    }
}

/// Host text we own CLIPBOARD with.
struct Owned {
    text: String,
    time: u32,
}

/// A conversion of the X owner's CLIPBOARD for the host.
struct Incoming {
    time: u32,
    target: u32,
    /// `Some` once the owner answered INCR: the chunks so far and their type.
    incr: Option<(u32, Vec<u8>)>,
    touched: Instant,
}

/// An INCR send to a requestor, one chunk per property deletion.
struct Outgoing {
    requestor: u32,
    property: u32,
    type_: u32,
    data: Vec<u8>,
    sent: usize,
    touched: Instant,
}

pub(super) struct Bridge<'a> {
    conn: Conn,
    pasteboard: &'a mut dyn Pasteboard,
    /// The setting, flipped from another thread.
    sync: &'a AtomicBool,
    /// The setting as last acted on.
    syncing: bool,
    atoms: Atoms,
    window: u32,
    xfixes_event: u8,
    owned: Option<Owned>,
    /// The pasteboard change count already handled: seen, or our own write.
    seen_count: i64,
    incoming: Option<Incoming>,
    outgoing: Vec<Outgoing>,
}

impl<'a> Bridge<'a> {
    /// Set up the bridge's window and selection input, then, syncing, take
    /// CLIPBOARD for the host's text when no X client owns it.
    pub(super) fn new(
        mut conn: Conn,
        pasteboard: &'a mut dyn Pasteboard,
        sync: &'a AtomicBool,
    ) -> io::Result<Self> {
        let atoms = Atoms::intern(&mut conn)?;
        let (xfixes, xfixes_event) = conn
            .query_extension("XFIXES")?
            .ok_or_else(|| io::Error::other("no XFIXES"))?;
        conn.xfixes_query_version(xfixes)?;
        let window = conn.create_window(PROPERTY_CHANGE_MASK)?;
        conn.xfixes_select_selection_input(xfixes, window, atoms.clipboard, XFIXES_SELECTION_MASK)?;
        let x_owner = conn.get_selection_owner(atoms.clipboard)?;
        let seen_count = pasteboard.change_count();
        let syncing = sync.load(Ordering::Acquire);
        let mut bridge = Self {
            conn,
            pasteboard,
            sync,
            syncing,
            atoms,
            window,
            xfixes_event,
            owned: None,
            seen_count,
            incoming: None,
            outgoing: Vec::new(),
        };
        if syncing
            && x_owner == 0
            && let Some(text) = bridge.pasteboard.read_text()
        {
            bridge.claim(text)?;
        }
        Ok(bridge)
    }

    /// Serve until the connection fails or closes.
    pub(super) fn run(&mut self) -> io::Result<()> {
        let mut next_poll = Instant::now() + POLL;
        loop {
            let sync = self.sync.load(Ordering::Acquire);
            if sync != self.syncing {
                self.syncing = sync;
                if sync {
                    self.sync_on()?;
                } else {
                    self.sync_off()?;
                }
            }
            let wait = next_poll.saturating_duration_since(Instant::now());
            if let Some(event) = self.conn.event(wait)? {
                self.handle(&event)?;
            }
            if Instant::now() >= next_poll {
                if self.syncing {
                    self.poll_host()?;
                }
                self.expire();
                next_poll = Instant::now() + POLL;
            }
        }
    }

    fn handle(&mut self, event: &Event) -> io::Result<()> {
        let code = event[0] & 0x7f;
        match code {
            SELECTION_REQUEST => self.selection_request(event),
            SELECTION_NOTIFY => self.selection_notify(event),
            SELECTION_CLEAR => {
                if u32_at(event, 12) == self.atoms.clipboard {
                    self.owned = None;
                }
                Ok(())
            }
            PROPERTY_NOTIFY => self.property_notify(event),
            _ if code == self.xfixes_event => self.owner_changed(event),
            _ => Ok(()),
        }
    }

    /// Sync turned off: drop our own conversion and give CLIPBOARD up.
    /// INCR sends already started run to their end: their requestors hold
    /// a SelectionNotify of type INCR, and §2.7.2 has no way to take that
    /// back. The transfer lives on the requestor's property, not on the
    /// selection, so ownership can go at once; requests still queued to us
    /// are refused by `selection_request`, later ones by the server.
    fn sync_off(&mut self) -> io::Result<()> {
        log::info!("clipboard: sync off");
        self.incoming = None;
        if self.owned.take().is_some() {
            let time = self.server_time()?;
            self.conn
                .set_selection_owner(0, self.atoms.clipboard, time)?;
        }
        Ok(())
    }

    /// Sync turned on: the host's text is the current content; what X
    /// copied meanwhile waits for X's next copy.
    fn sync_on(&mut self) -> io::Result<()> {
        log::info!("clipboard: sync on");
        self.seen_count = self.pasteboard.change_count();
        match self.pasteboard.read_text() {
            Some(text) => self.claim(text),
            None => Ok(()),
        }
    }

    /// The host pasteboard changed (not by us): own CLIPBOARD for its text,
    /// or give it up when the host holds no text.
    fn poll_host(&mut self) -> io::Result<()> {
        let count = self.pasteboard.change_count();
        if count == self.seen_count {
            return Ok(());
        }
        self.seen_count = count;
        match self.pasteboard.read_text() {
            Some(text) => self.claim(text),
            None if self.owned.take().is_some() => {
                let time = self.server_time()?;
                self.conn.set_selection_owner(0, self.atoms.clipboard, time)
            }
            None => Ok(()),
        }
    }

    /// Own CLIPBOARD with `text`, at a real server time (ICCCM §2.1).
    fn claim(&mut self, text: String) -> io::Result<()> {
        let time = self.server_time()?;
        self.conn
            .set_selection_owner(self.window, self.atoms.clipboard, time)?;
        if self.conn.get_selection_owner(self.atoms.clipboard)? == self.window {
            self.incoming = None;
            self.owned = Some(Owned { text, time });
        }
        Ok(())
    }

    /// The server time, from a zero-length append's `PropertyNotify`.
    fn server_time(&mut self) -> io::Result<u32> {
        let (window, property) = (self.window, self.atoms.time);
        self.conn
            .change_property(2, window, property, STRING, 8, &[])?;
        let event = self.conn.wait_event(|e| {
            e[0] & 0x7f == PROPERTY_NOTIFY && u32_at(e, 4) == window && u32_at(e, 8) == property
        })?;
        Ok(u32_at(&event, 12))
    }

    /// XFIXES `SelectionNotify`: convert a new X owner's text, or take
    /// CLIPBOARD back for the host's text when the owner went away.
    fn owner_changed(&mut self, event: &Event) -> io::Result<()> {
        let (owner, selection, selection_time) =
            (u32_at(event, 8), u32_at(event, 12), u32_at(event, 20));
        if !self.syncing || selection != self.atoms.clipboard || owner == self.window {
            return Ok(());
        }
        self.owned = None;
        if owner == 0 {
            self.incoming = None;
            if let Some(text) = self.pasteboard.read_text() {
                self.claim(text)?;
            }
            return Ok(());
        }
        self.convert(selection_time, self.atoms.utf8_string)
    }

    fn convert(&mut self, time: u32, target: u32) -> io::Result<()> {
        self.incoming = Some(Incoming {
            time,
            target,
            incr: None,
            touched: Instant::now(),
        });
        self.conn
            .delete_property(self.window, self.atoms.transfer)?;
        self.conn.convert_selection(
            self.window,
            self.atoms.clipboard,
            target,
            self.atoms.transfer,
            time,
        )
    }

    /// The owner answered our `ConvertSelection`.
    fn selection_notify(&mut self, event: &Event) -> io::Result<()> {
        // Not matched on time: not every owner echoes the request's.
        let (requestor, selection, target, property) = (
            u32_at(event, 8),
            u32_at(event, 12),
            u32_at(event, 16),
            u32_at(event, 20),
        );
        let Some(incoming) = &self.incoming else {
            return Ok(());
        };
        if requestor != self.window
            || selection != self.atoms.clipboard
            || target != incoming.target
            || incoming.incr.is_some()
        {
            return Ok(());
        }
        if property == 0 {
            let time = incoming.time;
            self.incoming = None;
            if target == self.atoms.utf8_string {
                return self.convert(time, STRING);
            }
            log::debug!("clipboard: the X owner has no text");
            return Ok(());
        }
        let Ok(reply) = self.conn.get_property(self.window, property, true)? else {
            self.incoming = None;
            return Ok(());
        };
        if reply.type_ == self.atoms.incr {
            // Deleting the property (above) started the transfer.
            if let Some(incoming) = &mut self.incoming {
                incoming.incr = Some((0, Vec::new()));
                incoming.touched = Instant::now();
            }
            return Ok(());
        }
        self.incoming = None;
        self.deliver(&reply);
        Ok(())
    }

    fn property_notify(&mut self, event: &Event) -> io::Result<()> {
        let (window, atom, new_value) = (u32_at(event, 4), u32_at(event, 8), event[16] == 0);
        if window == self.window && atom == self.atoms.transfer && new_value {
            return self.incr_chunk_in();
        }
        if !new_value
            && let Some(at) = self
                .outgoing
                .iter()
                .position(|o| o.requestor == window && o.property == atom)
        {
            return self.incr_chunk_out(at);
        }
        Ok(())
    }

    /// The owner appended an INCR chunk; a zero-length one ends it.
    fn incr_chunk_in(&mut self) -> io::Result<()> {
        if !self
            .incoming
            .as_ref()
            .is_some_and(|incoming| incoming.incr.is_some())
        {
            return Ok(());
        }
        let Ok(chunk) = self
            .conn
            .get_property(self.window, self.atoms.transfer, true)?
        else {
            self.incoming = None;
            return Ok(());
        };
        let Some((type_, data)) = self.incoming.as_mut().and_then(|incoming| {
            incoming.touched = Instant::now();
            incoming.incr.as_mut()
        }) else {
            return Ok(());
        };
        if !chunk.data.is_empty() {
            *type_ = chunk.type_;
            data.extend_from_slice(&chunk.data);
            return Ok(());
        }
        let whole = Property {
            type_: *type_,
            format: 8,
            bytes_after: 0,
            data: std::mem::take(data),
        };
        self.incoming = None;
        self.deliver(&whole);
        Ok(())
    }

    /// Hand converted text to the host, remembering the write's change count
    /// so its echo is not taken for a host change.
    fn deliver(&mut self, property: &Property) {
        let text = if property.type_ == self.atoms.utf8_string {
            String::from_utf8_lossy(&property.data).into_owned()
        } else if property.type_ == STRING {
            latin1_to_string(&property.data)
        } else {
            log::debug!("clipboard: X text of unsupported type {}", property.type_);
            return;
        };
        self.seen_count = self.pasteboard.write_text(&text);
    }

    /// A requestor asks for our CLIPBOARD (ICCCM §2.2).
    fn selection_request(&mut self, event: &Event) -> io::Result<()> {
        let (time, requestor, selection, target, property) = (
            u32_at(event, 4),
            u32_at(event, 12),
            u32_at(event, 16),
            u32_at(event, 20),
            u32_at(event, 24),
        );
        // A zero property is an obsolete client's: use the target (§2.2).
        let property = if property == 0 { target } else { property };
        let answered = match &self.owned {
            Some(owned)
                if selection == self.atoms.clipboard && (time == 0 || time >= owned.time) =>
            {
                if target == self.atoms.multiple {
                    self.convert_multiple(requestor, property)?
                } else {
                    self.convert_target(requestor, target, property, true)?
                }
            }
            _ => false,
        };
        self.conn.send_selection_notify(
            time,
            requestor,
            selection,
            target,
            if answered { property } else { 0 },
        )
    }

    /// The `(type, format, data)` of our text as `target`.
    fn target_data(&self, target: u32) -> Option<(u32, u8, Vec<u8>)> {
        let owned = self.owned.as_ref()?;
        let a = &self.atoms;
        if target == a.targets {
            let list = [
                a.targets,
                a.multiple,
                a.timestamp,
                a.utf8_string,
                STRING,
                a.text,
            ];
            return Some((
                ATOM,
                32,
                list.iter().flat_map(|t| t.to_le_bytes()).collect(),
            ));
        }
        if target == a.timestamp {
            return Some((INTEGER, 32, owned.time.to_le_bytes().to_vec()));
        }
        if target == a.utf8_string {
            return Some((a.utf8_string, 8, owned.text.as_bytes().to_vec()));
        }
        if target == STRING {
            return Some((STRING, 8, string_to_latin1(&owned.text)));
        }
        if target == a.text {
            // The owner picks the encoding: STRING when it holds the text.
            return Some(if owned.text.chars().all(|c| u32::from(c) <= 0xff) {
                (STRING, 8, string_to_latin1(&owned.text))
            } else {
                (a.utf8_string, 8, owned.text.as_bytes().to_vec())
            });
        }
        None
    }

    /// Write `target` to `requestor`'s `property`; INCR when it does not fit
    /// one request and `incr` allows it. False: refused.
    fn convert_target(
        &mut self,
        requestor: u32,
        target: u32,
        property: u32,
        incr: bool,
    ) -> io::Result<bool> {
        let Some((type_, format, data)) = self.target_data(target) else {
            return Ok(false);
        };
        let max = self.conn.max_request_bytes - CHANGE_PROPERTY_HEADER;
        if data.len() <= max {
            self.conn
                .change_property(0, requestor, property, type_, format, &data)?;
            return Ok(true);
        }
        if !incr {
            return Ok(false);
        }
        // §2.7.2: select PropertyNotify, write INCR with a lower bound on the
        // size, then a chunk after each deletion of the property.
        self.conn.select_input(requestor, PROPERTY_CHANGE_MASK)?;
        let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
        self.conn.change_property(
            0,
            requestor,
            property,
            self.atoms.incr,
            32,
            &size.to_le_bytes(),
        )?;
        self.outgoing
            .retain(|o| !(o.requestor == requestor && o.property == property));
        self.outgoing.push(Outgoing {
            requestor,
            property,
            type_,
            data,
            sent: 0,
            touched: Instant::now(),
        });
        Ok(true)
    }

    /// `MULTIPLE` (§2.6.2): convert each pair of the requestor's
    /// `ATOM_PAIR` property, replacing the target of a failed pair by None.
    fn convert_multiple(&mut self, requestor: u32, property: u32) -> io::Result<bool> {
        let Ok(pairs) = self.conn.get_property(requestor, property, false)? else {
            return Ok(false);
        };
        if pairs.format != 32 || pairs.data.is_empty() {
            return Ok(false);
        }
        let mut atoms: Vec<u32> = pairs.data.chunks_exact(4).map(|c| u32_at(c, 0)).collect();
        for pair in atoms.chunks_exact_mut(2) {
            if pair[1] == 0 || !self.convert_target(requestor, pair[0], pair[1], false)? {
                pair[0] = 0;
            }
        }
        let data: Vec<u8> = atoms.iter().flat_map(|a| a.to_le_bytes()).collect();
        self.conn
            .change_property(0, requestor, property, self.atoms.atom_pair, 32, &data)?;
        Ok(true)
    }

    /// The requestor deleted the INCR property: the next chunk, or the
    /// zero-length end.
    fn incr_chunk_out(&mut self, at: usize) -> io::Result<()> {
        let chunk_max = self.conn.max_request_bytes - CHANGE_PROPERTY_HEADER;
        let o = &mut self.outgoing[at];
        let end = (o.sent + chunk_max).min(o.data.len());
        let chunk = o.data[o.sent..end].to_vec();
        let (requestor, property, type_) = (o.requestor, o.property, o.type_);
        let done = chunk.is_empty();
        o.sent = end;
        o.touched = Instant::now();
        self.conn
            .change_property(0, requestor, property, type_, 8, &chunk)?;
        if done {
            self.outgoing.remove(at);
            if !self.outgoing.iter().any(|o| o.requestor == requestor) {
                self.conn.select_input(requestor, 0)?;
            }
        }
        Ok(())
    }

    /// Drop transfers the other side abandoned.
    fn expire(&mut self) {
        let stale = |t: Instant| t.elapsed() > TRANSFER_TIMEOUT;
        if self.incoming.as_ref().is_some_and(|i| stale(i.touched)) {
            log::debug!("clipboard: the X owner never finished a conversion");
            self.incoming = None;
        }
        let before = self.outgoing.len();
        self.outgoing.retain(|o| !stale(o.touched));
        if self.outgoing.len() < before {
            log::debug!("clipboard: dropped a stalled INCR send");
        }
    }
}

/// ISO Latin-1 (`STRING`) bytes as text.
pub(super) fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Text as ISO Latin-1 (`STRING`); characters past U+00FF become `?`.
pub(super) fn string_to_latin1(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}
