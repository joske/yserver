//! The clipboard: X's `CLIPBOARD` and `PRIMARY` selections and the Wayland
//! compositor's clipboard and primary selection, one in both directions
//! both ways.
//!
//! The Wayland side is `ext_data_control_v1`, the protocol of a clipboard
//! manager: it is told every selection whether or not one of the server's
//! windows has the keyboard, and it needs no input serial to set one. The
//! X side is the core's rootless selection window
//! (`yserver_core::rootless::selection`), which owns an X selection on the
//! compositor's behalf and asks X clients for theirs; what the selection
//! protocol sends it reaches this module through `take_selection_events`.
//!
//! **X to Wayland.** When an X client owns a selection, a data source
//! offering text is set as the compositor's. When a Wayland program pastes
//! from it, the server asks the X owner for `UTF8_STRING` (then `STRING`),
//! reads the answer off its window and writes it to the Wayland program's
//! pipe.
//!
//! **Wayland to X.** When the compositor names a selection offering text
//! that is not the server's own source, the server's window takes the X
//! selection. An X client's `ConvertSelection` then becomes a `receive` on
//! the compositor's offer, read from a pipe into the requestor's property,
//! and its `SelectionNotify`. `TARGETS` is answered at once.
//!
//! Text only, as the design says, and no `INCR`: a selection larger than
//! one property is refused in both directions. Every transfer is read and
//! written without blocking, as its pipe can take it: the pipes are watched
//! by an epoll descriptor of the bridge's, which the core loop polls
//! (`DisplayHost::wake_fd`). A transfer gives up after [`PATIENCE`].

use std::{
    collections::VecDeque,
    os::fd::{AsFd as _, AsRawFd as _, OwnedFd, RawFd},
    time::{Duration, Instant},
};

use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};

use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use yserver_core::{
    backend::Backend,
    resources::SERVER_OWNER,
    rootless::{SelectionEvent, selection as requests},
    server::ServerState,
};
use yserver_protocol::x11::{AtomId, ResourceId};

use super::client::{Client, ClipboardEvent};

/// The types text is offered and asked for as on the Wayland side, the
/// preferred first.
const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// A type only the server's own sources offer, by which it knows its own
/// selection coming back from the compositor.
const OWN: &str = "application/x-yserver-selection";

/// How long a transfer may take before it is given up.
const PATIENCE: Duration = Duration::from_secs(5);

/// How often the loop comes round while a transfer is under way, where the
/// pipes cannot be watched (no epoll descriptor could be made).
const POLL: Duration = Duration::from_millis(5);

/// The most a selection may hold: what one property of the server's takes.
const LIMIT: usize = 16 << 20;

/// The most pastes of the compositor's selections under way at once.
const PASTES: usize = 16;

/// The property on the server's window an X owner answers into.
const PROPERTY: &str = "_YSERVER_SELECTION";

/// The two selections, by index: X's `CLIPBOARD` and `PRIMARY`.
const NAMES: [&str; 2] = ["CLIPBOARD", "PRIMARY"];

/// A Wayland program's paste of an X selection, waiting its turn.
#[derive(Debug)]
struct Send {
    which: usize,
    fd: OwnedFd,
    asked: Instant,
}

/// The paste the server is asking the X owner for now.
#[derive(Debug)]
struct Converting {
    send: Send,
    /// What was asked, and when.
    asked: Asked,
}

/// One `ConvertSelection` of the server's. Its time is its own
/// ([`Clipboard::request_time`]), and the owner answers with it (ICCCM
/// §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Asked {
    time: u32,
    selection: AtomId,
    target: AtomId,
}

impl Asked {
    /// Whether a `SelectionNotify` is the answer to this, a refusal
    /// included: it has this request's time, selection and type. One with
    /// another time is the late answer to a transfer given up.
    fn answered_by(&self, time: u32, selection: AtomId, target: AtomId) -> bool {
        time == self.time && selection == self.selection && target == self.target
    }
}

/// An X client's paste of the compositor's selection, read from the pipe.
#[derive(Debug)]
struct Answer {
    which: usize,
    reader: OwnedFd,
    data: Vec<u8>,
    time: u32,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
    asked: Instant,
}

/// Data on its way to a Wayland program's pipe.
#[derive(Debug)]
struct Writing {
    which: usize,
    fd: OwnedFd,
    data: Vec<u8>,
    at: usize,
    asked: Instant,
}

/// The atoms the bridge uses, interned at its first sync.
#[derive(Clone, Copy, Debug)]
struct Atoms {
    selections: [AtomId; 2],
    utf8: AtomId,
    string: AtomId,
    text: AtomId,
    plain: AtomId,
    plain_utf8: AtomId,
    targets: AtomId,
    timestamp: AtomId,
    atom: AtomId,
    integer: AtomId,
    incr: AtomId,
    property: AtomId,
}

impl Atoms {
    fn intern(state: &mut ServerState) -> Self {
        let mut atom = |name: &str| state.atoms.intern(name, false);
        Self {
            selections: [atom(NAMES[0]), atom(NAMES[1])],
            utf8: atom("UTF8_STRING"),
            string: atom("STRING"),
            text: atom("TEXT"),
            plain: atom("text/plain"),
            plain_utf8: atom("text/plain;charset=utf-8"),
            targets: atom("TARGETS"),
            timestamp: atom("TIMESTAMP"),
            atom: atom("ATOM"),
            integer: atom("INTEGER"),
            incr: atom("INCR"),
            property: atom(PROPERTY),
        }
    }

    fn is_text(&self, target: AtomId) -> bool {
        [
            self.utf8,
            self.string,
            self.text,
            self.plain,
            self.plain_utf8,
        ]
        .contains(&target)
    }
}

/// The pipes of the transfers under way, watched for the core loop: its
/// descriptor is readable when one can move (data or its end to read, room
/// to write).
#[derive(Debug)]
struct Wake(Epoll);

impl Wake {
    fn new() -> nix::Result<Self> {
        Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC).map(Self)
    }

    fn fd(&self) -> RawFd {
        self.0.0.as_raw_fd()
    }

    /// Watch `fd` for reading or writing, until [`Wake::forget`].
    fn watch(&self, fd: &OwnedFd, writing: bool) {
        let flags = if writing {
            EpollFlags::EPOLLOUT
        } else {
            EpollFlags::EPOLLIN
        };
        if let Err(error) = self.0.add(fd, EpollEvent::new(flags, 0)) {
            log::warn!("wayland: watching a paste's pipe: {error}");
        }
    }

    /// Stop watching `fd`, before it is closed: another process may hold
    /// the pipe's end too, which would keep it watched.
    fn forget(&self, fd: &OwnedFd) {
        let _ = self.0.delete(fd);
    }

    /// Take what is ready: the pipes are read and written until they would
    /// block right after, and the core loop, edge-triggered, hears the next
    /// change.
    fn clear(&self) {
        let mut events = [EpollEvent::empty(); 8];
        let _ = self.0.wait(&mut events, EpollTimeout::ZERO);
    }
}

/// The bridge.
#[derive(Debug)]
pub(super) struct Clipboard {
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    atoms: Option<Atoms>,
    /// Every offer the compositor made and the types it said it has.
    offers: Vec<(ExtDataControlOfferV1, Vec<String>)>,
    /// The offer the compositor last named for each selection.
    named: [Option<ExtDataControlOfferV1>; 2],
    /// Whether it named one since the last sync.
    renamed: [bool; 2],
    /// The server's source standing for an X owner, for each selection.
    sources: [Option<ExtDataControlSourceV1>; 2],
    /// The X owner (window and time) the source stands for.
    mirrored: [Option<(ResourceId, u32)>; 2],
    /// X's owner of each selection (window and time), as the core last said
    /// (`SelectionEvent::Owner`).
    x_owner: [Option<(ResourceId, u32)>; 2],
    sends: VecDeque<Send>,
    converting: Option<Converting>,
    /// The time of the last `ConvertSelection`.
    asked_at: u32,
    answers: Vec<Answer>,
    writes: Vec<Writing>,
    /// `None` when no epoll descriptor could be made: the loop polls.
    wake: Option<Wake>,
}

impl Clipboard {
    /// Bind the compositor's data-control manager and make the seat's
    /// device; `None` when it offers neither.
    pub(super) fn bind(client: &Client) -> Option<Self> {
        let seat = client.seat()?;
        let manager: ExtDataControlManagerV1 = client.bind(1)?;
        let device = manager.get_data_device(seat, client.handle(), ());
        Some(Self {
            manager,
            device,
            atoms: None,
            offers: Vec::new(),
            named: [None, None],
            renamed: [false; 2],
            sources: [None, None],
            mirrored: [None; 2],
            x_owner: [None; 2],
            sends: VecDeque::new(),
            converting: None,
            asked_at: 0,
            answers: Vec::new(),
            writes: Vec::new(),
            wake: Wake::new()
                .inspect_err(|error| {
                    log::warn!("wayland: no epoll for the clipboard's pipes ({error}); polling");
                })
                .ok(),
        })
    }

    /// The descriptor the core loop polls for the transfers' pipes.
    pub(super) fn wake_fd(&self) -> Option<RawFd> {
        self.wake.as_ref().map(Wake::fd)
    }

    /// When the loop must come round for the bridge: a paste waiting to be
    /// asked for at once, else when the oldest transfer would be given up
    /// (its pipes wake the loop before that); without the epoll descriptor,
    /// every few milliseconds while one is under way.
    pub(super) fn deadline(&self, now: Instant) -> Option<Instant> {
        if self.converting.is_none() && !self.sends.is_empty() {
            return Some(now);
        }
        let oldest = self
            .converting
            .iter()
            .map(|converting| converting.send.asked)
            .chain(self.answers.iter().map(|answer| answer.asked))
            .chain(self.writes.iter().map(|writing| writing.asked))
            .min()?;
        Some(match self.wake {
            // Just past it: a transfer is given up once it took longer.
            Some(_) => oldest + PATIENCE + Duration::from_millis(1),
            None => now + POLL,
        })
    }

    /// An event of one of the bridge's objects.
    pub(super) fn event(&mut self, event: ClipboardEvent) {
        match event {
            ClipboardEvent::DataOffer(made) => self.offers.push((made, Vec::new())),
            ClipboardEvent::Offer { offer, mime } => {
                if let Some((_, types)) = self.offers.iter_mut().find(|(made, _)| *made == offer) {
                    types.push(mime);
                }
            }
            ClipboardEvent::Selection { which, offer } => self.name(which, offer),
            ClipboardEvent::Finished => {
                log::warn!("wayland: the compositor ended the clipboard's device");
            }
            ClipboardEvent::Send { source, fd } => {
                // A source already let go, whose last events are still
                // coming, has its descriptor closed as it is dropped.
                let Some(which) = self
                    .sources
                    .iter()
                    .position(|held| *held == Some(source.clone()))
                else {
                    return;
                };
                if let Err(error) = nonblocking(&fd) {
                    log::warn!("wayland: a paste's pipe: {error}");
                } else {
                    self.sends.push_back(Send {
                        which,
                        fd,
                        asked: Instant::now(),
                    });
                }
            }
            ClipboardEvent::Cancelled(source) => {
                let Some(which) = self
                    .sources
                    .iter()
                    .position(|held| *held == Some(source.clone()))
                else {
                    return;
                };
                // Something else is the selection now; the offer naming it
                // follows.
                source.destroy();
                self.sources[which] = None;
            }
        }
    }

    /// The compositor named `named` (an offer, or none) the selection
    /// `which` holds. The offer it replaces is let go.
    fn name(&mut self, which: usize, named: Option<ExtDataControlOfferV1>) {
        let old = std::mem::replace(&mut self.named[which], named);
        self.renamed[which] = true;
        if let Some(old) = old
            && self.named[which].as_ref() != Some(&old)
            && self.named[1 - which].as_ref() != Some(&old)
        {
            old.destroy();
            self.offers.retain(|(made, _)| *made != old);
        }
    }

    /// The types the offer `which` holds said it has.
    fn types(&self, which: usize) -> Option<&[String]> {
        let named = self.named[which].as_ref()?;
        self.offers
            .iter()
            .find(|(made, _)| made == named)
            .map(|(_, types)| types.as_slice())
    }

    /// Once per loop iteration: follow each side's selection on the other,
    /// and move what transfers can move.
    pub(super) fn sync(
        &mut self,
        client: &mut Client,
        state: &mut ServerState,
        backend: &mut dyn Backend,
    ) {
        self.follow_generation(state);
        let atoms = match self.atoms {
            Some(atoms) => atoms,
            None => self.watch(client, state),
        };
        let window = requests::selection_window(state);

        for which in [0, 1] {
            // The compositor's selection changed: an offer of text that is
            // not the server's own makes the server's window X's owner.
            if std::mem::take(&mut self.renamed[which]) {
                let types = self.types(which).map(<[String]>::to_vec);
                let own = types
                    .as_ref()
                    .is_some_and(|types| types.iter().any(|mime| mime == OWN));
                let text = types
                    .as_ref()
                    .is_some_and(|types| text_type(types).is_some());
                let ours = self.x_owner[which].is_some_and(|(owner, _)| owner == window);
                if !own && text {
                    log::info!(
                        "wayland: the compositor's {} ({}) is X's {} now",
                        if which == 0 {
                            "clipboard"
                        } else {
                            "primary selection"
                        },
                        types.unwrap_or_default().join(", "),
                        NAMES[which],
                    );
                    self.set_owner(state, which, true);
                } else if !own && (ours || (types.is_some() && self.x_owner[which].is_some())) {
                    // Nothing the bridge reads, or nothing at all, where
                    // its window stood for the compositor's selection; or
                    // something else than text copied over an X client's:
                    // that X client's is not what a paste gives any more.
                    log::info!(
                        "wayland: the compositor's {} holds no text; X's {} has no owner",
                        if which == 0 {
                            "clipboard"
                        } else {
                            "primary selection"
                        },
                        NAMES[which],
                    );
                    // The source that stood for the X owner is the
                    // compositor's no more: nothing of it is cleared when
                    // the owner goes.
                    self.mirrored[which] = None;
                    self.set_owner(state, which, false);
                }
            }
        }

        for event in requests::take_selection_events(state) {
            match event {
                SelectionEvent::Request {
                    time,
                    requestor,
                    selection,
                    target,
                    property,
                } => {
                    let property = if property.0 == 0 { target } else { property };
                    self.asked(
                        client, state, backend, &atoms, time, requestor, selection, target,
                        property,
                    );
                }
                SelectionEvent::Notify {
                    time,
                    selection,
                    target,
                    property,
                } => {
                    self.answered(state, &atoms, window, time, [selection, target, property]);
                }
                // No INCR: a property's changes are not followed, and no
                // requestor is watched.
                SelectionEvent::Property { .. } | SelectionEvent::Destroyed { .. } => {}
                // The Owner event that comes with it says who has it now.
                SelectionEvent::Clear { .. } => {}
                SelectionEvent::Owner {
                    selection,
                    owner,
                    time,
                } => {
                    if let Some(which) = atoms.selections.iter().position(|s| *s == selection) {
                        self.x_owner[which] = owner.map(|owner| (owner, time));
                        self.follow_x_owner(client, state, which);
                    }
                }
            }
        }

        if let Some(wake) = &self.wake {
            wake.clear();
        }
        self.convert(state, &atoms);
        self.read_answers(state, backend, &atoms);
        self.write_pastes();
    }

    /// A server reset took the generation the bridge knew, with its atoms,
    /// its window's watch of the selections, X's owners and the windows
    /// that asked: what the bridge held of it goes, and the next
    /// [`Clipboard::watch`] starts on the new one, where the compositor's
    /// selections are X's again.
    fn follow_generation(&mut self, state: &ServerState) {
        if self
            .atoms
            .is_none_or(|atoms| state.atoms.name(atoms.property) == Some(PROPERTY))
        {
            return;
        }
        log::info!("wayland: the server was reset; the clipboard starts again");
        self.atoms = None;
        self.x_owner = [None; 2];
        self.mirrored = [None; 2];
        // The pastes that asked X's owners end with nothing.
        self.converting = None;
        for answer in self.answers.drain(..) {
            if let Some(wake) = &self.wake {
                wake.forget(&answer.reader);
            }
        }
        for which in [0, 1] {
            if let Some(held) = self.sources[which].take() {
                self.set_wayland(which, None);
                held.destroy();
            }
        }
        self.renamed = [true; 2];
    }

    /// The first sync: intern the atoms, hear each change of the two
    /// selections' owners from now on, and follow who owns them already.
    fn watch(&mut self, client: &Client, state: &mut ServerState) -> Atoms {
        let atoms = Atoms::intern(state);
        self.atoms = Some(atoms);
        for which in [0, 1] {
            let selection = atoms.selections[which];
            if let Err(error) = requests::watch_selection(state, selection, true) {
                log::warn!("wayland: watching {}: {error:?}", NAMES[which]);
            }
            self.x_owner[which] = state
                .selections
                .get(&selection)
                .filter(|owner| owner.window.0 != 0)
                .map(|owner| (owner.window, owner.last_time_changed));
            self.follow_x_owner(client, state, which);
        }
        atoms
    }

    /// An X client owns the selection `which`: offer it to the compositor.
    /// No X client does: the source standing for one goes.
    fn follow_x_owner(&mut self, client: &Client, state: &mut ServerState, which: usize) {
        let window = requests::selection_window(state);
        match self.x_owner[which] {
            Some((owner, time))
                if owner != window && state.resources.window_owner(owner) != Some(SERVER_OWNER) =>
            {
                if self.mirrored[which] != Some((owner, time)) {
                    self.mirror(client, which);
                    self.mirrored[which] = Some((owner, time));
                    log::info!(
                        "wayland: X's {} (0x{:x}) is the compositor's {} now",
                        NAMES[which],
                        owner.0,
                        if which == 0 {
                            "clipboard"
                        } else {
                            "primary selection"
                        },
                    );
                }
            }
            Some(_) => self.mirrored[which] = None,
            None => {
                if self.mirrored[which].take().is_some()
                    && let Some(held) = self.sources[which].take()
                {
                    // The X owner went, and what it held with it.
                    self.set_wayland(which, None);
                    held.destroy();
                }
            }
        }
    }

    /// Make the server's selection window the owner of X's selection
    /// `which`, or no one.
    fn set_owner(&self, state: &mut ServerState, which: usize, own: bool) {
        let Some(atoms) = self.atoms else {
            return;
        };
        if let Err(error) = requests::set_selection_owner(state, atoms.selections[which], own) {
            log::warn!("wayland: owning {}: {error:?}", NAMES[which]);
        }
    }

    /// A new source of the server's, offering text, as the compositor's
    /// selection `which`.
    fn mirror(&mut self, client: &Client, which: usize) {
        let made = self.manager.create_data_source(client.handle(), ());
        // Not as `STRING`, which is Latin-1: what is written is UTF-8.
        for mime in TEXT_TYPES
            .iter()
            .filter(|mime| **mime != "STRING")
            .chain(&[OWN])
        {
            made.offer((*mime).to_owned());
        }
        if let Some(old) = self.sources[which].replace(made.clone()) {
            old.destroy();
        }
        self.set_wayland(which, Some(&made));
    }

    /// Set the compositor's selection `which` to `source`, or clear it.
    fn set_wayland(&self, which: usize, source: Option<&ExtDataControlSourceV1>) {
        if which == 0 {
            self.device.set_selection(source);
        } else {
            self.device.set_primary_selection(source);
        }
    }

    /// An X client asked the server's window for the selection it owns on
    /// the compositor's behalf.
    #[expect(
        clippy::too_many_arguments,
        reason = "a SelectionRequest's own fields, and the state to answer it with"
    )]
    fn asked(
        &mut self,
        client: &mut Client,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        atoms: &Atoms,
        time: u32,
        requestor: ResourceId,
        selection: AtomId,
        target: AtomId,
        property: AtomId,
    ) {
        let refuse = |state: &mut ServerState| {
            notify(state, time, requestor, [selection, target, AtomId(0)]);
        };
        let Some(which) = atoms.selections.iter().position(|held| *held == selection) else {
            refuse(state);
            return;
        };
        if target == atoms.timestamp {
            // When the server's window took the selection, which every
            // owner answers (ICCCM 2.6.2).
            let taken = self.x_owner[which].map_or(0, |(_, time)| time);
            if let Err(error) = requests::change_property(
                state,
                backend,
                requestor,
                [property, atoms.integer],
                32,
                taken.to_ne_bytes().to_vec(),
            ) {
                log::warn!("wayland: answering TIMESTAMP: {error:?}");
                refuse(state);
                return;
            }
            notify(state, time, requestor, [selection, target, property]);
            return;
        }
        if target == atoms.targets {
            let list: Vec<u8> = [
                atoms.targets,
                atoms.timestamp,
                atoms.utf8,
                atoms.string,
                atoms.text,
                atoms.plain_utf8,
            ]
            .iter()
            .flat_map(|atom| atom.0.to_ne_bytes())
            .collect();
            if let Err(error) = requests::change_property(
                state,
                backend,
                requestor,
                [property, atoms.atom],
                32,
                list,
            ) {
                log::warn!("wayland: answering TARGETS: {error:?}");
                refuse(state);
                return;
            }
            notify(state, time, requestor, [selection, target, property]);
            return;
        }
        let offered = self.named[which]
            .clone()
            .zip(self.types(which).and_then(text_type).map(str::to_owned));
        let (Some((held, mime)), true) = (offered, atoms.is_text(target)) else {
            refuse(state);
            return;
        };
        // An X client can ask as often as it likes, and each paste under
        // way holds a pipe and what it read so far.
        if self.answers.len() >= PASTES {
            log::warn!(
                "wayland: {PASTES} pastes of the compositor's {} under way; one more refused",
                NAMES[which]
            );
            refuse(state);
            return;
        }
        let (reader, writer) = match paste_pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                log::warn!("wayland: a pipe for a paste: {error}");
                refuse(state);
                return;
            }
        };
        held.receive(mime.clone(), writer.as_fd());
        let asked = client.flush();
        // The compositor has its own copy of the writing end now; this one
        // closed is how the reader sees the end of the data.
        drop(writer);
        if let Err(error) = asked {
            log::warn!("wayland: asking the compositor for {mime}: {error}");
            refuse(state);
            return;
        }
        if let Some(wake) = &self.wake {
            wake.watch(&reader, false);
        }
        self.answers.push(Answer {
            which,
            reader,
            data: Vec::new(),
            time,
            requestor,
            selection,
            target,
            property,
            asked: Instant::now(),
        });
    }

    /// Read what the compositor's selections gave, and answer the X client
    /// once all of it has come.
    fn read_answers(&mut self, state: &mut ServerState, backend: &mut dyn Backend, atoms: &Atoms) {
        let mut done = Vec::new();
        for (at, answer) in self.answers.iter_mut().enumerate() {
            let mut chunk = [0u8; 65536];
            let ended = loop {
                match nix::unistd::read(&answer.reader, &mut chunk) {
                    Ok(0) => break Some(true),
                    Ok(read) => {
                        answer
                            .data
                            .extend_from_slice(chunk.get(..read).unwrap_or_default());
                        if answer.data.len() > LIMIT {
                            log::warn!(
                                "wayland: the compositor's {} is larger than {LIMIT} bytes, \
                                 which X takes only as INCR",
                                NAMES[answer.which]
                            );
                            break Some(false);
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => {
                        break (answer.asked.elapsed() > PATIENCE).then_some(false);
                    }
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => break Some(false),
                }
            };
            if let Some(whole) = ended {
                done.push((at, whole));
            }
        }
        for (at, whole) in done.into_iter().rev() {
            let answer = self.answers.remove(at);
            if let Some(wake) = &self.wake {
                wake.forget(&answer.reader);
            }
            let (r#type, data) = if answer.target == atoms.string {
                (atoms.string, latin1(&answer.data))
            } else {
                (atoms.utf8, answer.data)
            };
            let len = data.len();
            let put = whole
                && requests::change_property(
                    state,
                    backend,
                    answer.requestor,
                    [answer.property, r#type],
                    8,
                    data,
                )
                .inspect_err(|error| log::warn!("wayland: answering a paste: {error:?}"))
                .is_ok();
            if put {
                log::info!(
                    "wayland: gave an X client {} bytes of the compositor's {} as {}",
                    len,
                    NAMES[answer.which],
                    state.atoms.name(answer.target).unwrap_or("?"),
                );
            } else {
                log::warn!(
                    "wayland: the compositor's {} did not reach the X client",
                    NAMES[answer.which]
                );
            }
            notify(
                state,
                answer.time,
                answer.requestor,
                [
                    answer.selection,
                    answer.target,
                    if put { answer.property } else { AtomId(0) },
                ],
            );
        }
    }

    /// Ask the X owner for the next paste waiting, when none is being
    /// asked for; give up on one that took too long.
    fn convert(&mut self, state: &mut ServerState, atoms: &Atoms) {
        if let Some(converting) = &self.converting
            && converting.send.asked.elapsed() > PATIENCE
        {
            log::warn!(
                "wayland: X's {} owner did not answer",
                NAMES[converting.send.which]
            );
            self.converting = None;
        }
        if self.converting.is_some() {
            return;
        }
        let Some(send) = self.sends.pop_front() else {
            return;
        };
        self.ask(state, atoms, send, atoms.utf8);
    }

    /// The time of the next `ConvertSelection`: the server's, or a
    /// millisecond after the last request's when the clock has not moved
    /// since, so no two requests share a time. Never `CurrentTime`.
    fn request_time(&mut self, now: u32) -> u32 {
        self.asked_at = now.max(self.asked_at.wrapping_add(1)).max(1);
        self.asked_at
    }

    /// Ask the X owner for `send`'s selection as `target`, into the
    /// bridge's property, which holds nothing as the owner is asked
    /// (ICCCM §2.4).
    fn ask(&mut self, state: &mut ServerState, atoms: &Atoms, send: Send, target: AtomId) {
        let selection = atoms.selections[send.which];
        let window = requests::selection_window(state);
        state
            .resources
            .delete_window_property(window, atoms.property);
        let time = self.request_time(state.timestamp_now());
        match requests::convert_selection(state, selection, target, atoms.property, time) {
            Ok(()) => {
                let asked = Asked {
                    time,
                    selection,
                    target,
                };
                self.converting = Some(Converting { send, asked });
            }
            Err(error) => log::warn!("wayland: asking for {}: {error:?}", NAMES[send.which]),
        }
    }

    /// An X owner answered the server's window: the transfer being asked
    /// for, or a late answer to one given up.
    fn answered(
        &mut self,
        state: &mut ServerState,
        atoms: &Atoms,
        window: ResourceId,
        time: u32,
        [selection, target, property]: [AtomId; 3],
    ) {
        let Some(converting) = self
            .converting
            .take_if(|converting| converting.asked.answered_by(time, selection, target))
        else {
            log::debug!(
                "wayland: a late answer for {} ({}), dropped",
                state.atoms.name(selection).unwrap_or("?"),
                state.atoms.name(target).unwrap_or("?"),
            );
            // Its data goes, when it is in the bridge's property and no
            // transfer is waiting for an answer there: any client can send
            // the server's window a SelectionNotify naming any property.
            if property == atoms.property && self.converting.is_none() {
                state.resources.delete_window_property(window, property);
            }
            return;
        };
        if property != atoms.property {
            // Refused: an owner that has no UTF8_STRING may have STRING.
            if target == atoms.utf8 {
                self.ask(state, atoms, converting.send, atoms.string);
            } else {
                log::warn!(
                    "wayland: X's {} owner refused it as text",
                    NAMES[converting.send.which]
                );
            }
            return;
        }
        let Some(value) = state.resources.delete_window_property(window, property) else {
            log::warn!("wayland: X's selection owner said it answered, and did not");
            return;
        };
        if value.r#type == atoms.incr {
            // The Wayland program's pipe closes with nothing in it.
            log::warn!(
                "wayland: X's {} came as INCR, which the bridge does not take: \
                 the paste is empty",
                NAMES[converting.send.which]
            );
            return;
        }
        let data = if value.r#type == atoms.string {
            latin1_to_utf8(&value.data)
        } else {
            value.data
        };
        if let Some(wake) = &self.wake {
            wake.watch(&converting.send.fd, true);
        }
        self.writes.push(Writing {
            which: converting.send.which,
            fd: converting.send.fd,
            data,
            at: 0,
            asked: converting.send.asked,
        });
    }

    /// Write what X's owners gave to the pipes of the Wayland programs that
    /// asked, each closed once all of it is written.
    fn write_pastes(&mut self) {
        let wake = self.wake.as_ref();
        self.writes.retain_mut(|writing| {
            let keep = write_paste(writing);
            if !keep && let Some(wake) = wake {
                wake.forget(&writing.fd);
            }
            keep
        });
    }
}

/// Write what is left of a paste until its pipe would block: whether it
/// waits for room.
fn write_paste(writing: &mut Writing) -> bool {
    loop {
        let Some(rest) = writing.data.get(writing.at..) else {
            return false;
        };
        if rest.is_empty() {
            log::info!(
                "wayland: gave the compositor {} bytes of X's {}",
                writing.data.len(),
                NAMES[writing.which]
            );
            return false;
        }
        let written = nix::unistd::write(&writing.fd, rest);
        match written {
            Ok(count) => writing.at += count,
            Err(nix::errno::Errno::EAGAIN) => {
                return writing.asked.elapsed() <= PATIENCE;
            }
            Err(nix::errno::Errno::EINTR) => {}
            Err(error) => {
                log::warn!("wayland: writing a paste: {error}");
                return false;
            }
        }
    }
}

/// Tell an X requestor where its answer is.
fn notify(state: &mut ServerState, time: u32, requestor: ResourceId, answer: [AtomId; 3]) {
    if let Err(error) = requests::send_selection_notify(state, time, requestor, answer) {
        log::warn!("wayland: answering a paste: {error:?}");
    }
}

/// The type to ask an offer of `types` for, when it has text the bridge
/// reads: the first of [`TEXT_TYPES`] it has, as the offer spells it (a
/// charset is written in either case). Text in another charset is none:
/// the bridge hands X what it reads as UTF-8.
fn text_type(types: &[String]) -> Option<&str> {
    TEXT_TYPES.iter().find_map(|text| {
        types
            .iter()
            .find(|mime| text.eq_ignore_ascii_case(mime))
            .map(String::as_str)
    })
}

/// A pipe for the compositor's selection to be written into: the reading
/// end the server's, not blocking; the writing end goes to the selection's
/// owner, which may write blocking and must not see `EAGAIN`; `O_NONBLOCK`
/// given to `pipe2` would be both ends'.
fn paste_pipe() -> nix::Result<(OwnedFd, OwnedFd)> {
    let (reader, writer) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
    nonblocking(&reader)?;
    Ok((reader, writer))
}

/// Make a pipe's end not block.
fn nonblocking(fd: &OwnedFd) -> nix::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(drop)
}

/// UTF-8 text as ICCCM `STRING`, Latin-1: what has no Latin-1 form is a
/// question mark.
fn latin1(text: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(text)
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// ICCCM `STRING`, Latin-1, as UTF-8.
fn latin1_to_utf8(text: &[u8]) -> Vec<u8> {
    text.iter()
        .map(|&byte| char::from(byte))
        .collect::<String>()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use yserver_core::properties::{PropertyFormat, PropertyValue};

    use super::*;

    /// A `SelectionNotify` is the answer to the transfer asked for when it
    /// has its time, selection and type: a late answer, or a late refusal,
    /// has an earlier request's time, whatever else it names.
    #[test]
    fn an_answer_is_matched_to_its_transfer() {
        let (clipboard, primary) = (AtomId(10), AtomId(11));
        let (utf8, string) = (AtomId(20), AtomId(21));
        let asked = Asked {
            time: 5001,
            selection: clipboard,
            target: utf8,
        };
        assert!(asked.answered_by(5001, clipboard, utf8));
        assert!(
            !asked.answered_by(5000, clipboard, utf8),
            "the answer to an earlier request for the same"
        );
        assert!(!asked.answered_by(0, clipboard, utf8), "no request's time");
        assert!(!asked.answered_by(5001, primary, utf8), "another selection");
        assert!(!asked.answered_by(5001, clipboard, string), "another type");
    }

    /// A bridge with no compositor behind it, and a server.
    fn bridge() -> (Clipboard, ServerState, Atoms) {
        use wayland_client::{Connection, Proxy as _};
        let (host, _compositor) = std::os::unix::net::UnixStream::pair().unwrap();
        let wire = Connection::from_socket(host).unwrap().backend().downgrade();
        let mut state = ServerState::new();
        let atoms = Atoms::intern(&mut state);
        let bridge = Clipboard {
            manager: ExtDataControlManagerV1::inert(wire.clone()),
            device: ExtDataControlDeviceV1::inert(wire),
            atoms: Some(atoms),
            offers: Vec::new(),
            named: [None, None],
            renamed: [false; 2],
            sources: [None, None],
            mirrored: [None; 2],
            x_owner: [None; 2],
            sends: VecDeque::new(),
            converting: None,
            asked_at: 0,
            answers: Vec::new(),
            writes: Vec::new(),
            wake: None,
        };
        (bridge, state, atoms)
    }

    /// Put `text` in `property` of the server's selection window.
    fn put(state: &mut ServerState, atoms: &Atoms, property: AtomId, text: &[u8]) {
        let window = requests::selection_window(state);
        state.resources.set_window_property(
            window,
            property,
            PropertyValue {
                r#type: atoms.utf8,
                format: PropertyFormat::F8,
                data: text.to_vec(),
            },
        );
    }

    /// A Wayland program's paste of X's CLIPBOARD, and its end of the pipe.
    fn paste() -> (Send, OwnedFd) {
        let (reader, writer) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
        let send = Send {
            which: 0,
            fd: writer,
            asked: Instant::now(),
        };
        (send, reader)
    }

    /// The answer to a transfer given up has that transfer's time, and is
    /// not the answer to the one asked for since: a late refusal does not
    /// end it or start its STRING fallback, and a late answer is not read
    /// as its data. The data of a late answer goes only from the bridge's
    /// own property, and not while a transfer waits for an answer there.
    #[test]
    fn a_late_answer_is_not_the_answer_to_a_newer_transfer() {
        let (mut bridge, mut state, atoms) = bridge();
        let window = requests::selection_window(&mut state);
        let clipboard = atoms.selections[0];
        let refused = [clipboard, atoms.utf8, AtomId(0)];
        let answered = [clipboard, atoms.utf8, atoms.property];

        // One paste is asked for and given up, and the next is asked for.
        let (first, _first_reader) = paste();
        bridge.ask(&mut state, &atoms, first, atoms.utf8);
        let late = bridge.converting.take().unwrap().asked.time;
        let (second, _second_reader) = paste();
        bridge.ask(&mut state, &atoms, second, atoms.utf8);
        let asked = bridge.converting.as_ref().unwrap().asked;
        assert_ne!(asked.time, late, "asked in the same millisecond");

        // The first owner's refusal comes now: the second transfer is
        // still asked for, as UTF8_STRING.
        bridge.answered(&mut state, &atoms, window, late, refused);
        assert_eq!(bridge.converting.as_ref().map(|c| c.asked), Some(asked));

        // So does the first owner's answer, after the second's owner put
        // its data in the property: it is not taken, and the data stays.
        put(&mut state, &atoms, atoms.property, b"second");
        bridge.answered(&mut state, &atoms, window, late, answered);
        assert_eq!(bridge.converting.as_ref().map(|c| c.asked), Some(asked));
        assert!(bridge.writes.is_empty());
        assert!(
            state
                .resources
                .window_property(window, atoms.property)
                .is_some()
        );

        // The second's own answer is taken, and its data read.
        bridge.answered(&mut state, &atoms, window, asked.time, answered);
        assert!(bridge.converting.is_none());
        assert_eq!(bridge.writes.len(), 1);
        assert_eq!(bridge.writes[0].data, b"second");
        assert!(
            state
                .resources
                .window_property(window, atoms.property)
                .is_none()
        );

        // With no transfer waiting, a late answer's data goes from the
        // bridge's property, and from no other a client may name.
        let other = state.atoms.intern("_NOT_THE_BRIDGES", false);
        put(&mut state, &atoms, atoms.property, b"late");
        put(&mut state, &atoms, other, b"kept");
        bridge.answered(
            &mut state,
            &atoms,
            window,
            late,
            [clipboard, atoms.utf8, other],
        );
        bridge.answered(&mut state, &atoms, window, late, answered);
        assert!(
            state
                .resources
                .window_property(window, atoms.property)
                .is_none()
        );
        assert!(state.resources.window_property(window, other).is_some());
        assert!(bridge.converting.is_none());
        assert_eq!(bridge.writes.len(), 1);
    }

    /// A server reset: the bridge lets go of what it held of the generation
    /// that went (its atoms, X's owners, the transfer asked for), so that
    /// its next sync watches the new one, and takes the compositor's
    /// selections for X again. Nothing changes while the generation stands.
    #[test]
    fn a_server_reset_starts_the_bridge_again() {
        let (mut bridge, mut state, atoms) = bridge();
        let (send, _reader) = paste();
        bridge.ask(&mut state, &atoms, send, atoms.utf8);
        bridge.x_owner[0] = Some((ResourceId(0x0020_0001), 5));
        bridge.mirrored[0] = bridge.x_owner[0];
        bridge.follow_generation(&state);
        assert!(bridge.atoms.is_some(), "the same generation");
        assert!(bridge.converting.is_some());

        // The new generation's atoms are not the old one's.
        let reset = ServerState::new();
        bridge.follow_generation(&reset);
        assert!(bridge.atoms.is_none(), "watched again at the next sync");
        assert_eq!(bridge.x_owner, [None; 2]);
        assert_eq!(bridge.mirrored, [None; 2]);
        assert!(bridge.converting.is_none());
        assert_eq!(bridge.renamed, [true; 2]);
    }

    /// An owner that refuses UTF8_STRING is asked for STRING, as a new
    /// request with a time of its own; refused again, the paste ends.
    #[test]
    fn a_refused_utf8_string_is_asked_for_as_string() {
        let (mut bridge, mut state, atoms) = bridge();
        let window = requests::selection_window(&mut state);
        let clipboard = atoms.selections[0];
        let (send, _reader) = paste();
        bridge.ask(&mut state, &atoms, send, atoms.utf8);
        let first = bridge.converting.as_ref().unwrap().asked;
        bridge.answered(
            &mut state,
            &atoms,
            window,
            first.time,
            [clipboard, atoms.utf8, AtomId(0)],
        );
        let second = bridge.converting.as_ref().unwrap().asked;
        assert_eq!(second.target, atoms.string);
        assert_ne!(second.time, first.time);
        bridge.answered(
            &mut state,
            &atoms,
            window,
            second.time,
            [clipboard, atoms.string, AtomId(0)],
        );
        assert!(bridge.converting.is_none());
        assert!(bridge.writes.is_empty());
    }

    /// The bridge's descriptor is readable once a watched pipe has data,
    /// and not once it is forgotten.
    #[test]
    fn the_wake_descriptor_is_readable_when_a_pipe_can_move() {
        use std::os::fd::BorrowedFd;

        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        let wake = Wake::new().unwrap();
        let readable = |wake: &Wake| {
            // SAFETY: the descriptor is `wake`'s, open for this call.
            let fd = unsafe { BorrowedFd::borrow_raw(wake.fd()) };
            let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
            poll(&mut fds, PollTimeout::ZERO).unwrap() == 1
        };
        let (reader, writer) = paste_pipe().unwrap();
        wake.watch(&reader, false);
        assert!(!readable(&wake), "nothing to read yet");
        nix::unistd::write(&writer, b"text").unwrap();
        assert!(readable(&wake));
        wake.forget(&reader);
        wake.clear();
        assert!(!readable(&wake), "forgotten");
    }

    /// Only the server's end of a paste's pipe does not block: the owner
    /// writing into the other must not be cut short by `EAGAIN`.
    #[test]
    fn a_paste_pipe_blocks_only_at_the_owners_end() {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let (reader, writer) = paste_pipe().unwrap();
        let flags = |fd: &OwnedFd| OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).unwrap());
        assert!(flags(&reader).contains(OFlag::O_NONBLOCK));
        assert!(!flags(&writer).contains(OFlag::O_NONBLOCK));
        let cloexec = |fd: &OwnedFd| fcntl(fd, FcntlArg::F_GETFD).unwrap() & libc::FD_CLOEXEC;
        assert_ne!(cloexec(&reader), 0);
        assert_ne!(cloexec(&writer), 0);
    }

    #[test]
    fn text_goes_between_utf8_and_latin1() {
        assert_eq!(latin1("café €".as_bytes()), b"caf\xe9 ?");
        assert_eq!(latin1_to_utf8(b"caf\xe9"), "café".as_bytes());
    }

    /// An offer has text when it has one of the text types, which a paste
    /// is then asked for as the offer spells it, the preferred first. Text
    /// in a charset the bridge does not read as UTF-8 is none, nor is the
    /// server's own marker.
    #[test]
    fn text_is_a_type_the_bridge_can_ask_for() {
        let types = |list: &[&str]| list.iter().map(|&mime| mime.to_owned()).collect::<Vec<_>>();
        for (offered, asked) in [
            (
                &["text/plain;charset=utf-8"][..],
                Some("text/plain;charset=utf-8"),
            ),
            (
                &["text/plain;charset=UTF-8"],
                Some("text/plain;charset=UTF-8"),
            ),
            (
                &["STRING", "text/plain", "UTF8_STRING"],
                Some("UTF8_STRING"),
            ),
            (&["text/plain;charset=iso-8859-1"], None),
            (&["image/png", OWN], None),
            (&[], None),
        ] {
            let offered = types(offered);
            assert_eq!(text_type(&offered), asked, "{offered:?}");
        }
    }
}
