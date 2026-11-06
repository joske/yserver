//! The connection to the compositor, on `wayland-client`'s own Rust
//! implementation of the wire (no libwayland).
//!
//! [`Client`] owns the connection and its one event queue, binds the globals
//! every window needs (`wl_compositor`, `wl_shm`, `wl_seat`, `xdg_wm_base`,
//! the outputs and their `zxdg_output_v1`, `wp_cursor_shape_manager_v1`),
//! and keeps what the protocol makes a client keep: the configure
//! handshake, each surface's `wl_shm` buffers and which of them the
//! compositor still reads, the seat's serials and focus, the cursor. What
//! the host acts on comes out of [`Client::dispatch`] as [`Event`]s, in the
//! order the compositor sent them: a relative motion still comes before the
//! pointer motion it belongs to.
//!
//! The globals only one part of the host uses (the relative pointer, the
//! pointer constraints, data control) are that part's to bind, with
//! [`Client::bind`]; their events come back as [`Event`]s too.

use std::{
    collections::HashMap,
    io,
    num::NonZeroUsize,
    os::fd::{AsFd as _, AsRawFd as _, OwnedFd, RawFd},
    ptr::NonNull,
};

use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::WaylandError,
    event_created_child,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    ext::data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
        ext_data_control_source_v1::{self, ExtDataControlSourceV1},
    },
    wp::{
        cursor_shape::v1::client::{
            wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
            wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        },
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
            zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::{
        shell::client::{
            xdg_popup::{self, XdgPopup},
            xdg_positioner::{self, XdgPositioner},
            xdg_surface::{self, XdgSurface},
            xdg_toplevel::{self, XdgToplevel},
            xdg_wm_base::{self, XdgWmBase},
        },
        xdg_output::zv1::client::{
            zxdg_output_manager_v1::ZxdgOutputManagerV1,
            zxdg_output_v1::{self, ZxdgOutputV1},
        },
    },
};
use xkbcommon::xkb;

/// The versions bound at most: what the host speaks.
const COMPOSITOR: u32 = 6;
const SEAT: u32 = 8;
const WM_BASE: u32 = 3;
const OUTPUT: u32 = 4;
const XDG_OUTPUT: u32 = 3;

/// `wl_shm`'s formats the host draws in.
const ARGB8888: wl_shm::Format = wl_shm::Format::Argb8888;
const XRGB8888: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// A window or popup the host made, by its own number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(super) struct SurfaceId(u32);

#[cfg(test)]
impl SurfaceId {
    /// A number no client made, for the host's tests.
    pub(super) const fn for_test(raw: u32) -> Self {
        Self(raw)
    }
}

/// A rectangle in surface coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) struct Rect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}

/// Where a popup goes: `size` with its top left corner at `anchor_rect`'s,
/// growing to the bottom right, and the compositor asked not to move it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Placement {
    pub(super) size: (i32, i32),
    pub(super) anchor_rect: Rect,
}

/// What a window is made with, before its first commit.
#[derive(Clone, Debug, Default)]
pub(super) struct ToplevelOptions {
    pub(super) title: String,
    pub(super) app_id: String,
    /// The size it asks for, where the compositor leaves the choice to it.
    pub(super) size: (u32, u32),
    /// The window it is a dialog of.
    pub(super) parent: Option<SurfaceId>,
    pub(super) min_size: (u32, u32),
    pub(super) max_size: (u32, u32),
}

/// One screen as the compositor described it.
#[derive(Clone, Debug, Default)]
pub(super) struct Output {
    mode: (i32, i32),
    pub(super) refresh_mhz: i32,
    scale: i32,
    sideways: bool,
    logical_extent: Option<(i32, i32)>,
    pub(super) done: bool,
}

impl Output {
    /// Its size in logical pixels: `zxdg_output_v1`'s where it said one,
    /// else the mode turned by the transform and divided by the scale.
    pub(super) fn logical_size(&self) -> (i32, i32) {
        if let Some(extent) = self.logical_extent {
            return extent;
        }
        let (width, height) = if self.sideways {
            (self.mode.1, self.mode.0)
        } else {
            self.mode
        };
        let scale = self.scale.max(1);
        (width / scale, height / scale)
    }
}

/// The pointer, as the compositor reports it on the host's windows.
#[derive(Clone, Copy, Debug)]
pub(super) enum PointerEvent {
    Enter {
        surface: SurfaceId,
        x: f64,
        y: f64,
    },
    Leave,
    Motion {
        surface: SurfaceId,
        x: f64,
        y: f64,
    },
    /// At the last position given.
    Button {
        surface: SurfaceId,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
    },
    /// One `wl_pointer.frame`'s scroll: continuous, and whole clicks
    /// (`axis_value120` summed to clicks, or `axis_discrete`), down and
    /// right.
    Axis {
        vertical: f64,
        horizontal: f64,
        discrete: (i32, i32),
    },
}

/// The keyboard on the host's windows.
#[derive(Clone, Copy, Debug)]
pub(super) enum KeyboardEvent {
    Enter(SurfaceId),
    Leave,
    /// An evdev code, as the compositor sends it: no repeats.
    Key {
        code: u32,
        pressed: bool,
    },
}

/// What the data control objects said ([`super::clipboard`]).
#[derive(Debug)]
pub(super) enum ClipboardEvent {
    /// A new offer, whose types follow.
    DataOffer(ExtDataControlOfferV1),
    /// One type an offer has.
    Offer {
        offer: ExtDataControlOfferV1,
        mime: String,
    },
    /// The selection (0, the clipboard) or primary selection (1) is now
    /// `offer`, or nothing.
    Selection {
        which: usize,
        offer: Option<ExtDataControlOfferV1>,
    },
    /// The device is no longer valid.
    Finished,
    /// A program pastes the host's source into `fd`.
    Send {
        source: ExtDataControlSourceV1,
        fd: OwnedFd,
    },
    /// The source is no longer the selection.
    Cancelled(ExtDataControlSourceV1),
}

/// What happened on the connection, for the host.
#[derive(Debug)]
pub(super) enum Event {
    /// A frame callback: the compositor wants the next frame.
    Frame(SurfaceId),
    /// The configure handshake is done (acknowledged): draw at this size.
    /// A window's zero is the size it has, else the size it asked for.
    Configure {
        surface: SurfaceId,
        width: u32,
        height: u32,
    },
    /// The compositor took a popup away (`xdg_popup.popup_done`).
    Closed(SurfaceId),
    /// The compositor asks a window to close (`xdg_toplevel.close`).
    CloseRequested(SurfaceId),
    Pointer(PointerEvent),
    Keyboard(KeyboardEvent),
    /// `zwp_relative_pointer_v1.relative_motion`: the device's time in
    /// microseconds of `CLOCK_MONOTONIC`, and the motion.
    RelativeMotion {
        utime: u64,
        delta: (f64, f64),
        unaccelerated: (f64, f64),
    },
    /// `zwp_locked_pointer_v1.locked` or `unlocked`.
    Lock {
        lock: ZwpLockedPointerV1,
        locked: bool,
    },
    Clipboard(ClipboardEvent),
}

/// What a `wl_surface` of the host's is.
#[derive(Clone, Copy, Debug)]
enum SurfaceData {
    Window(SurfaceId),
    Cursor,
}

/// What a `wl_buffer` of the host's is.
#[derive(Clone, Copy, Debug)]
enum BufferData {
    /// One of a window's `wl_shm` buffers.
    Window(SurfaceId),
    Cursor,
}

/// Memory shared with the compositor: a memfd, mapped.
#[derive(Debug)]
struct Shared {
    fd: OwnedFd,
    map: NonNull<std::ffi::c_void>,
    len: NonZeroUsize,
}

impl Shared {
    fn new(len: usize) -> io::Result<Self> {
        let len = NonZeroUsize::new(len).ok_or_else(|| io::Error::other("an empty buffer"))?;
        let fd = nix::sys::memfd::memfd_create(
            c"yserver-wayland",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )?;
        nix::unistd::ftruncate(&fd, i64::try_from(len.get()).map_err(io::Error::other)?)?;
        // SAFETY: a fresh shared mapping of a memfd this process made and
        // sized to `len`; nothing else maps it in this process, and it is
        // unmapped once, on drop.
        let map = unsafe {
            nix::sys::mman::mmap(
                None,
                len,
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                &fd,
                0,
            )?
        };
        Ok(Self { fd, map, len })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `map` is `len` bytes, mapped read-write for as long as
        // `self` lives, and borrowed mutably through `self` alone. The
        // compositor reads the same pages, which is the point; it writes
        // none.
        unsafe { std::slice::from_raw_parts_mut(self.map.as_ptr().cast(), self.len.get()) }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        if let Err(error) = unsafe { nix::sys::mman::munmap(self.map, self.len.get()) } {
            log::warn!("wayland: unmapping a buffer: {error}");
        }
    }
}

/// A pool and a buffer over new shared memory, `width` × `height` at four
/// bytes a pixel.
fn shm_buffer(
    shm: &WlShm,
    qh: &QueueHandle<State>,
    (width, height): (u32, u32),
    format: wl_shm::Format,
    data: BufferData,
) -> io::Result<(WlShmPool, WlBuffer, Shared)> {
    let len = length(width, height).ok_or_else(|| io::Error::other("a buffer too large"))?;
    let shared = Shared::new(len)?;
    let pool = shm.create_pool(shared.fd.as_fd(), int(len), qh, ());
    let buffer = pool.create_buffer(
        0,
        int(width),
        int(height),
        int(width.saturating_mul(4)),
        format,
        qh,
        data,
    );
    Ok((pool, buffer, shared))
}

/// The bytes of a `width` × `height` buffer at four bytes a pixel, `None`
/// for an empty or impossibly large one.
fn length(width: u32, height: u32) -> Option<usize> {
    let len = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    (len > 0 && i32::try_from(len).is_ok()).then_some(len)
}

/// One `wl_shm` buffer of a window's.
#[derive(Debug)]
struct Slot {
    shared: Shared,
    pool: WlShmPool,
    buffer: WlBuffer,
    width: u32,
    height: u32,
    format: wl_shm::Format,
    /// Attached and not released: the compositor may still read it.
    busy: bool,
}

impl Slot {
    fn destroy(self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// What a surface is for.
#[derive(Debug)]
enum Role {
    Popup {
        xdg: XdgSurface,
        popup: XdgPopup,
        parent: SurfaceId,
        /// The size the last `xdg_popup.configure` gave.
        pending: (u32, u32),
    },
    Toplevel {
        xdg: XdgSurface,
        toplevel: XdgToplevel,
        /// The size asked for.
        size: (u32, u32),
        /// The size the last `xdg_toplevel.configure` gave; a zero leaves
        /// it to the host.
        pending: (u32, u32),
    },
}

impl Role {
    fn xdg(&self) -> &XdgSurface {
        match self {
            Self::Popup { xdg, .. } | Self::Toplevel { xdg, .. } => xdg,
        }
    }
}

/// One window or popup.
#[derive(Debug)]
struct Surface {
    surface: WlSurface,
    role: Role,
    configured: Option<(u32, u32)>,
    slots: Vec<Slot>,
}

/// A cursor the host drew: a surface of the seat's own and the one buffer
/// it shows.
#[derive(Debug)]
struct CursorImage {
    buffer: WlBuffer,
    pool: WlShmPool,
    /// Kept mapped until the buffer is replaced.
    _shared: Shared,
    hot: (i32, i32),
}

/// An axis event being put together until `wl_pointer.frame`.
#[derive(Clone, Copy, Debug, Default)]
struct Axis {
    vertical: f64,
    horizontal: f64,
    discrete: (i32, i32),
}

/// The seat's state.
#[derive(Debug, Default)]
struct Seat {
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    /// The layout and variant of each group the keymap has, where the XKB
    /// rules name it.
    layouts: Vec<Option<(String, String)>>,
    group: u32,
    pointer_focus: Option<SurfaceId>,
    pointer_at: (f64, f64),
    enter_serial: u32,
    /// The last button or key press's serial, which a drag needs.
    input_serial: u32,
    hidden: bool,
    cursor_surface: Option<WlSurface>,
    image: Option<CursorImage>,
    axis: Option<Axis>,
    /// What `axis_value120` sent that is not yet a whole click.
    residue: (i32, i32),
}

/// One global the registry announced.
#[derive(Clone, Debug)]
struct Global {
    name: u32,
    interface: String,
    version: u32,
}

/// One screen's objects beside its description.
#[derive(Debug)]
struct OutputState {
    global: u32,
    output: WlOutput,
    xdg: Option<ZxdgOutputV1>,
    info: Output,
}

/// What the event queue's handlers keep.
#[derive(Debug, Default)]
pub(super) struct State {
    globals: Vec<Global>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    xdg_outputs: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputState>,
    surfaces: HashMap<SurfaceId, Surface>,
    seat: Seat,
    pending: Vec<Event>,
}

/// The connection to the compositor, and everything bound on it.
pub(super) struct Client {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    registry: WlRegistry,
    state: State,
    next_surface: u32,
}

impl Client {
    /// Connect to the compositor `WAYLAND_DISPLAY` names, bind what every
    /// window needs, and wait until the screens, the seat's devices and the
    /// keymap have been described.
    ///
    /// # Errors
    ///
    /// No display, a socket that refuses, or a compositor that closes the
    /// connection during the first round trips.
    pub(super) fn connect() -> io::Result<Self> {
        let connection = Connection::connect_to_env().map_err(io::Error::other)?;
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut client = Self {
            connection,
            queue,
            qh,
            registry,
            state: State::default(),
            next_surface: 1,
        };
        // The globals, then what binding them made the compositor say (the
        // screens' descriptions, the seat's devices), then what those made
        // it say (the keymap, the xdg outputs).
        for _ in 0..3 {
            client.roundtrip()?;
        }
        Ok(client)
    }

    /// Bind the first global of `I` at the lower of `version` and what the
    /// compositor offers; `None` when it offers none.
    pub(super) fn bind<I>(&self, version: u32) -> Option<I>
    where
        I: Proxy + 'static,
        State: Dispatch<I, ()>,
    {
        let interface = I::interface();
        let global = self
            .state
            .globals
            .iter()
            .find(|global| global.interface == interface.name)?;
        let version = version.min(global.version).min(interface.version).max(1);
        Some(
            self.registry
                .bind::<I, (), State>(global.name, version, &self.qh, ()),
        )
    }

    /// The queue the host's own objects' events go to.
    pub(super) fn handle(&self) -> &QueueHandle<State> {
        &self.qh
    }

    /// The seat, once the compositor has one.
    pub(super) fn seat(&self) -> Option<&WlSeat> {
        self.state.seat.seat.as_ref()
    }

    /// The seat's pointer, once it has one.
    pub(super) fn pointer(&self) -> Option<&WlPointer> {
        self.state.seat.pointer.as_ref()
    }

    /// The `wl_surface` of a window or popup.
    pub(super) fn surface(&self, surface: SurfaceId) -> Option<&WlSurface> {
        self.state
            .surfaces
            .get(&surface)
            .map(|state| &state.surface)
    }

    /// Every screen that has been described, in the order they came.
    pub(super) fn outputs(&self) -> impl Iterator<Item = &Output> {
        self.state
            .outputs
            .iter()
            .map(|output| &output.info)
            .filter(|output| output.done)
    }

    /// The layout and variant of the keyboard's group in force, once a
    /// keymap has come whose layouts the XKB rules name.
    pub(super) fn keyboard_layout(&self) -> Option<(String, String)> {
        let layouts = &self.state.seat.layouts;
        usize::try_from(self.state.seat.group)
            .ok()
            .and_then(|group| layouts.get(group))
            .cloned()
            .flatten()
            .or_else(|| layouts.iter().flatten().next().cloned())
    }

    /// The connection's descriptor, which the core loop polls.
    pub(super) fn as_raw_fd(&self) -> RawFd {
        self.connection.backend().poll_fd().as_raw_fd()
    }

    /// Send what is queued, read what the socket holds without waiting, and
    /// hand back everything that happened.
    ///
    /// # Errors
    ///
    /// The compositor closing the connection or refusing a request.
    pub(super) fn dispatch(&mut self) -> io::Result<Vec<Event>> {
        self.flush()?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(io::Error::other)?;
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(io::Error::other)?;
        Ok(std::mem::take(&mut self.state.pending))
    }

    /// `wl_display.sync` and dispatch until it is answered: everything the
    /// compositor had to say about what was sent before it has been said.
    ///
    /// # Errors
    ///
    /// As [`Client::dispatch`].
    pub(super) fn roundtrip(&mut self) -> io::Result<Vec<Event>> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(io::Error::other)?;
        Ok(std::mem::take(&mut self.state.pending))
    }

    /// Send what is queued. A full socket is not an error: the rest goes
    /// with the next flush.
    ///
    /// # Errors
    ///
    /// Writing to the compositor failing.
    pub(super) fn flush(&self) -> io::Result<()> {
        match self.connection.flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(io::Error::other(error)),
        }
    }

    /// A `wl_surface` for the window or popup `id`, when the compositor
    /// shell can give it a role.
    fn new_surface(&self, id: SurfaceId) -> io::Result<WlSurface> {
        self.wm_base()?;
        let compositor = self
            .state
            .compositor
            .as_ref()
            .ok_or_else(|| io::Error::other("the compositor offers no wl_compositor"))?;
        Ok(compositor.create_surface(&self.qh, SurfaceData::Window(id)))
    }

    fn wm_base(&self) -> io::Result<&XdgWmBase> {
        self.state
            .wm_base
            .as_ref()
            .ok_or_else(|| io::Error::other("the compositor offers no xdg_wm_base"))
    }

    /// A window: an `xdg_toplevel` with `options`' title, app id, parent and
    /// size limits, committed bare so the compositor configures it. A
    /// [`Event::Configure`] follows.
    ///
    /// # Errors
    ///
    /// No `wl_compositor` or `xdg_wm_base`.
    pub(super) fn toplevel(&mut self, options: &ToplevelOptions) -> io::Result<SurfaceId> {
        let id = SurfaceId(self.next_surface);
        let surface = self.new_surface(id)?;
        self.next_surface = self.next_surface.wrapping_add(1);
        let base = self.wm_base()?;
        let xdg = base.get_xdg_surface(&surface, &self.qh, id);
        let toplevel = xdg.get_toplevel(&self.qh, id);
        // Before the first commit, where a compositor decides whether the
        // window floats.
        if let Some(parent) = options.parent.and_then(|parent| self.toplevel_of(parent)) {
            toplevel.set_parent(Some(parent));
        }
        toplevel.set_title(options.title.clone());
        toplevel.set_app_id(options.app_id.clone());
        if options.min_size != (0, 0) || options.max_size != (0, 0) {
            size_limits(&toplevel, options.min_size, options.max_size);
        }
        surface.commit();
        self.state.surfaces.insert(
            id,
            Surface {
                surface,
                role: Role::Toplevel {
                    xdg,
                    toplevel,
                    size: options.size,
                    pending: (0, 0),
                },
                configured: None,
                slots: Vec::new(),
            },
        );
        Ok(id)
    }

    /// An `xdg_popup` on the window or popup `parent`, placed as `placement`
    /// says and committed bare. No grab is taken.
    ///
    /// # Errors
    ///
    /// No `xdg_wm_base`, or no such parent.
    pub(super) fn popup(
        &mut self,
        parent: SurfaceId,
        placement: &Placement,
    ) -> io::Result<SurfaceId> {
        let id = SurfaceId(self.next_surface);
        self.next_surface = self.next_surface.wrapping_add(1);
        self.make_popup(id, parent, placement)?;
        Ok(id)
    }

    fn make_popup(
        &mut self,
        id: SurfaceId,
        parent: SurfaceId,
        placement: &Placement,
    ) -> io::Result<()> {
        let parent_xdg = self
            .state
            .surfaces
            .get(&parent)
            .map(|state| state.role.xdg().clone())
            .ok_or_else(|| io::Error::other("a popup's parent must be a window or a popup"))?;
        let surface = self.new_surface(id)?;
        let base = self.wm_base()?;
        let positioner = positioner(base, &self.qh, placement);
        let xdg = base.get_xdg_surface(&surface, &self.qh, id);
        let popup = xdg.get_popup(Some(&parent_xdg), &positioner, &self.qh, id);
        positioner.destroy();
        surface.commit();
        self.state.surfaces.insert(
            id,
            Surface {
                surface,
                role: Role::Popup {
                    xdg,
                    popup,
                    parent,
                    pending: (0, 0),
                },
                configured: None,
                slots: Vec::new(),
            },
        );
        Ok(())
    }

    fn toplevel_of(&self, window: SurfaceId) -> Option<&XdgToplevel> {
        match self.state.surfaces.get(&window).map(|state| &state.role) {
            Some(Role::Toplevel { toplevel, .. }) => Some(toplevel),
            _ => None,
        }
    }

    /// Change a window's title. Anything but a window is left alone.
    pub(super) fn set_title(&self, window: SurfaceId, title: &str) {
        if let Some(toplevel) = self.toplevel_of(window) {
            toplevel.set_title(title.to_owned());
        }
    }

    /// Change a window's app id. Anything but a window is left alone.
    pub(super) fn set_app_id(&self, window: SurfaceId, app_id: &str) {
        if let Some(toplevel) = self.toplevel_of(window) {
            toplevel.set_app_id(app_id.to_owned());
        }
    }

    /// Change the least and greatest size a window may be given (0 is no
    /// limit), in effect from its next commit.
    pub(super) fn set_size_limits(&self, window: SurfaceId, min: (u32, u32), max: (u32, u32)) {
        if let Some(toplevel) = self.toplevel_of(window) {
            size_limits(toplevel, min, max);
        }
    }

    /// Make a window a dialog of another, or, with `None`, a window of its
    /// own again. Anything but two windows is left alone.
    pub(super) fn set_parent(&self, window: SurfaceId, parent: Option<SurfaceId>) {
        let Some(toplevel) = self.toplevel_of(window) else {
            return;
        };
        match parent {
            Some(parent) => {
                if let Some(parent) = self.toplevel_of(parent) {
                    toplevel.set_parent(Some(parent));
                }
            }
            None => toplevel.set_parent(None),
        }
    }

    /// Hand the last press to the compositor, to move the window by the
    /// pointer (`edges` 0) or resize it by the edges named (an
    /// `xdg_toplevel.resize_edge`). The compositor may decline.
    pub(super) fn drag_window(&self, window: SurfaceId, edges: u32) {
        let (Some(toplevel), Some(seat)) = (self.toplevel_of(window), self.seat()) else {
            return;
        };
        let serial = self.state.seat.input_serial;
        if edges == 0 {
            toplevel._move(seat, serial);
        } else if let Ok(edges) = xdg_toplevel::ResizeEdge::try_from(edges) {
            toplevel.resize(seat, serial, edges);
        }
    }

    /// Move a popup (`xdg_popup.reposition`, version 3); where the
    /// compositor's `xdg_wm_base` is older, the popup is made again at the
    /// new place under the same [`SurfaceId`].
    ///
    /// # Errors
    ///
    /// Making it again failed.
    pub(super) fn reposition_popup(
        &mut self,
        id: SurfaceId,
        placement: &Placement,
    ) -> io::Result<()> {
        let Some(Surface {
            role: Role::Popup { popup, parent, .. },
            ..
        }) = self.state.surfaces.get(&id)
        else {
            return Ok(());
        };
        if popup.version() >= 3 {
            let positioner = positioner(self.wm_base()?, &self.qh, placement);
            popup.reposition(&positioner, 0);
            positioner.destroy();
            return Ok(());
        }
        // Remade: the same number for the host, new objects underneath.
        let parent = *parent;
        self.destroy(id);
        self.make_popup(id, parent, placement)
    }

    /// Destroy a window or popup, with its role and its buffers.
    pub(super) fn destroy(&mut self, surface: SurfaceId) {
        let Some(state) = self.state.surfaces.remove(&surface) else {
            return;
        };
        match state.role {
            Role::Popup { xdg, popup, .. } => {
                popup.destroy();
                xdg.destroy();
            }
            Role::Toplevel { xdg, toplevel, .. } => {
                toplevel.destroy();
                xdg.destroy();
            }
        }
        for slot in state.slots {
            slot.destroy();
        }
        state.surface.destroy();
        if self.state.seat.pointer_focus == Some(surface) {
            self.state.seat.pointer_focus = None;
        }
    }

    /// Copy a frame of the host's size into a free `wl_shm` buffer of the
    /// surface's, written by `fill`, attach it and damage it: rows of
    /// `width` pixels, each B, G, R and alpha in memory, an X `ZPixmap` at
    /// 32 bits a pixel. An `opaque` frame's alpha is ignored (`XRGB8888`),
    /// otherwise it is premultiplied (`ARGB8888`). [`Client::commit_frame`]
    /// shows it.
    ///
    /// `fill` must write the whole buffer, which holds whatever an earlier
    /// frame left there, and answers what changed since the frame before as
    /// `(x, y, width, height)`, empty for all of it; `None` when it wrote
    /// nothing, and nothing is attached.
    ///
    /// Nothing is drawn before the first [`Event::Configure`], and a size of
    /// zero draws nothing; both answer `Ok(false)`.
    ///
    /// # Errors
    ///
    /// Shared memory that cannot be made.
    pub(super) fn draw_pixels(
        &mut self,
        surface: SurfaceId,
        (width, height): (u32, u32),
        opaque: bool,
        fill: impl FnOnce(&mut [u8]) -> Option<Vec<(u32, u32, u32, u32)>>,
    ) -> io::Result<bool> {
        let Some(shm) = self.state.shm.as_ref() else {
            return Err(io::Error::other("the compositor offers no wl_shm"));
        };
        let Some(state) = self
            .state
            .surfaces
            .get_mut(&surface)
            .filter(|state| state.configured.is_some())
        else {
            return Ok(false);
        };
        if length(width, height).is_none() {
            return Ok(false);
        }
        let format = if opaque { XRGB8888 } else { ARGB8888 };
        // A free buffer of this size, made if need be; free ones of another
        // size go.
        let (keep, stale): (Vec<Slot>, Vec<Slot>) = state.slots.drain(..).partition(|slot| {
            slot.busy || (slot.width == width && slot.height == height && slot.format == format)
        });
        state.slots = keep;
        for slot in stale {
            slot.destroy();
        }
        let at = match state.slots.iter().position(|slot| !slot.busy) {
            Some(at) => at,
            None => {
                let (pool, buffer, shared) = shm_buffer(
                    shm,
                    &self.qh,
                    (width, height),
                    format,
                    BufferData::Window(surface),
                )?;
                state.slots.push(Slot {
                    shared,
                    pool,
                    buffer,
                    width,
                    height,
                    format,
                    busy: false,
                });
                state.slots.len() - 1
            }
        };
        let Some(slot) = state.slots.get_mut(at) else {
            return Ok(false);
        };
        let Some(damage) = fill(slot.shared.bytes_mut()) else {
            return Ok(false);
        };
        slot.busy = true;
        state.surface.attach(Some(&slot.buffer), 0, 0);
        let whole = [(0, 0, width, height)];
        let rects = if damage.is_empty() {
            &whole[..]
        } else {
            &damage
        };
        for &(x, y, width, height) in rects {
            damage_buffer(&state.surface, (x, y, width, height));
        }
        Ok(true)
    }

    /// Ask for an [`Event::Frame`] when the compositor next wants a frame
    /// of the surface, and commit what was attached with it.
    pub(super) fn commit_frame(&self, surface: SurfaceId) {
        let Some(state) = self.state.surfaces.get(&surface) else {
            return;
        };
        state.surface.frame(&self.qh, surface);
        state.surface.commit();
    }

    /// Show the compositor's own arrow for the pointer
    /// (`wp_cursor_shape_v1`, where it serves that), in place of any
    /// picture of the host's.
    pub(super) fn set_default_cursor(&mut self) {
        let seat = &mut self.state.seat;
        seat.hidden = false;
        if let Some(image) = seat.image.take() {
            image.buffer.destroy();
            image.pool.destroy();
        }
        if let Some(surface) = seat.cursor_surface.take() {
            surface.destroy();
        }
        seat.apply_cursor();
    }

    /// Show a picture of the host's for the pointer over its surfaces
    /// (`wl_pointer.set_cursor` with a surface): `pixels` is `width` ×
    /// `height` in `wl_shm`'s premultiplied ARGB8888, `hot` the point of it
    /// that is the pointer's position. It stays, across the pointer leaving
    /// and coming back, until the next call.
    ///
    /// # Errors
    ///
    /// A size of zero, `pixels` of another length, or shared memory that
    /// cannot be made.
    pub(super) fn set_cursor_image(
        &mut self,
        width: u32,
        height: u32,
        hot: (i32, i32),
        pixels: &[u8],
    ) -> io::Result<()> {
        length(width, height)
            .filter(|&len| len == pixels.len())
            .ok_or_else(|| {
                io::Error::other(format!(
                    "a {width}x{height} cursor of {} bytes",
                    pixels.len()
                ))
            })?;
        let (Some(compositor), Some(shm)) =
            (self.state.compositor.as_ref(), self.state.shm.as_ref())
        else {
            return Err(io::Error::other("the compositor offers no wl_shm"));
        };
        let seat = &mut self.state.seat;
        let surface = seat
            .cursor_surface
            .get_or_insert_with(|| compositor.create_surface(&self.qh, SurfaceData::Cursor));
        let (pool, buffer, mut shared) =
            shm_buffer(shm, &self.qh, (width, height), ARGB8888, BufferData::Cursor)?;
        shared.bytes_mut().copy_from_slice(pixels);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, int(width), int(height));
        surface.commit();
        let image = CursorImage {
            buffer,
            pool,
            _shared: shared,
            hot,
        };
        if let Some(old) = seat.image.replace(image) {
            old.buffer.destroy();
            old.pool.destroy();
        }
        seat.hidden = false;
        seat.apply_cursor();
        Ok(())
    }

    /// Hide the pointer over the host's surfaces (`wl_pointer.set_cursor`
    /// with no surface).
    pub(super) fn hide_cursor(&mut self) {
        self.state.seat.hidden = true;
        self.state.seat.apply_cursor();
    }
}

impl State {
    fn surface_of(surface: &WlSurface) -> Option<SurfaceId> {
        match surface.data::<SurfaceData>() {
            Some(SurfaceData::Window(id)) => Some(*id),
            _ => None,
        }
    }

    fn configured(&mut self, surface: SurfaceId, width: u32, height: u32) {
        if let Some(state) = self.surfaces.get_mut(&surface) {
            state.configured = Some((width, height));
            self.pending.push(Event::Configure {
                surface,
                width,
                height,
            });
        }
    }

    fn add_output(
        &mut self,
        name: u32,
        version: u32,
        registry: &WlRegistry,
        qh: &QueueHandle<Self>,
    ) {
        let output = registry.bind::<WlOutput, u32, Self>(name, version.min(OUTPUT), qh, name);
        let xdg = self
            .xdg_outputs
            .as_ref()
            .map(|manager| manager.get_xdg_output(&output, qh, name));
        self.outputs.push(OutputState {
            global: name,
            output,
            xdg,
            info: Output {
                scale: 1,
                ..Output::default()
            },
        });
    }

    fn output_mut(&mut self, name: u32) -> Option<&mut Output> {
        self.outputs
            .iter_mut()
            .find(|output| output.global == name)
            .map(|output| &mut output.info)
    }

    fn flush_axis(&mut self) {
        let Some(axis) = self.seat.axis.take() else {
            return;
        };
        if self.seat.pointer_focus.is_some() {
            self.pending.push(Event::Pointer(PointerEvent::Axis {
                vertical: axis.vertical,
                horizontal: axis.horizontal,
                discrete: axis.discrete,
            }));
        }
    }
}

impl Seat {
    /// Give the compositor the cursor in force, when the pointer is on one
    /// of the host's surfaces: none, the host's picture, or the arrow.
    fn apply_cursor(&self) {
        let Some(pointer) = self
            .pointer
            .as_ref()
            .filter(|_| self.pointer_focus.is_some())
        else {
            return;
        };
        let serial = self.enter_serial;
        if self.hidden {
            pointer.set_cursor(serial, None, 0, 0);
        } else if let (Some(surface), Some(image)) = (&self.cursor_surface, &self.image) {
            pointer.set_cursor(serial, Some(surface), image.hot.0, image.hot.1);
        } else if let Some(device) = &self.cursor_device {
            device.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
        }
    }

    /// The layout and variant the XKB rules give each group of a keymap.
    fn read_keymap(&mut self, fd: OwnedFd, size: u32) {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let Ok(size) = usize::try_from(size) else {
            return;
        };
        // SAFETY: the descriptor came with the event and is this process's
        // alone; the compositor does not change a keymap it has sent, and
        // the map is private and read-only.
        let keymap = unsafe {
            xkb::Keymap::new_from_fd(
                &context,
                fd,
                size,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        let keymap = match keymap {
            Ok(Some(keymap)) => keymap,
            Ok(None) => {
                log::warn!("wayland: the compositor's keymap does not compile");
                return;
            }
            Err(error) => {
                log::warn!("wayland: reading the compositor's keymap: {error}");
                return;
            }
        };
        let rules = rules_list(&context);
        self.layouts = (0..keymap.num_layouts())
            .map(|group| {
                let label = keymap.layout_get_name(group);
                let named = rules
                    .as_deref()
                    .and_then(|rules| layout_named(rules, label));
                if named.is_none() {
                    log::info!("wayland: the XKB rules name no layout {label:?}");
                }
                named
            })
            .collect();
    }
}

/// The XKB rules' list of layouts and variants on the context's include
/// path: `evdev.lst`, else `base.lst`.
fn rules_list(context: &xkb::Context) -> Option<String> {
    context.include_paths().find_map(|dir| {
        ["evdev.lst", "base.lst"]
            .iter()
            .find_map(|file| std::fs::read_to_string(dir.join("rules").join(file)).ok())
    })
}

/// The layout and variant a rules list (`evdev.lst`) calls `label`, the
/// name a keymap gives its group: `German` is `de`, `German (no dead
/// keys)` is `de` `nodeadkeys`.
fn layout_named(rules: &str, label: &str) -> Option<(String, String)> {
    let mut section = "";
    for line in rules.lines() {
        if let Some(name) = line.strip_prefix('!') {
            section = name.trim();
            continue;
        }
        let Some((name, description)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let description = description.trim();
        match section {
            "layout" if description == label => return Some((name.to_owned(), String::new())),
            "variant" => {
                if let Some((layout, description)) = description.split_once(": ")
                    && description == label
                {
                    return Some((layout.to_owned(), name.to_owned()));
                }
            }
            _ => {}
        }
    }
    None
}

/// An `xdg_positioner` for `placement`: anchored at its rectangle's top
/// left, growing to the bottom right, not to be moved.
fn positioner(base: &XdgWmBase, qh: &QueueHandle<State>, placement: &Placement) -> XdgPositioner {
    let positioner = base.create_positioner(qh, ());
    positioner.set_size(placement.size.0.max(1), placement.size.1.max(1));
    let rect = placement.anchor_rect;
    positioner.set_anchor_rect(rect.x, rect.y, rect.width.max(1), rect.height.max(1));
    positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::empty());
    positioner.set_offset(0, 0);
    positioner
}

fn size_limits(toplevel: &XdgToplevel, min: (u32, u32), max: (u32, u32)) {
    toplevel.set_min_size(int(min.0), int(min.1));
    toplevel.set_max_size(int(max.0), int(max.1));
}

/// Damage in buffer pixels, where the surface takes that (version 4), else
/// in surface coordinates, which are the same at buffer scale 1.
fn damage_buffer(surface: &WlSurface, (x, y, width, height): (u32, u32, u32, u32)) {
    if surface.version() >= 4 {
        surface.damage_buffer(int(x), int(y), int(width), int(height));
    } else {
        surface.damage(int(x), int(y), int(width), int(height));
    }
}

/// A count as the `int` the wire wants.
fn int<T: TryInto<i32>>(value: T) -> i32 {
    value.try_into().unwrap_or(i32::MAX)
}

fn nonnegative(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

// -- the handlers ------------------------------------------------------------

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.globals.push(Global {
                    name,
                    interface: interface.clone(),
                    version,
                });
                match interface.as_str() {
                    "wl_compositor" if state.compositor.is_none() => {
                        state.compositor =
                            Some(registry.bind(name, version.min(COMPOSITOR), qh, ()));
                    }
                    "wl_shm" if state.shm.is_none() => {
                        state.shm = Some(registry.bind(name, 1, qh, ()));
                    }
                    "wl_seat" if state.seat.seat.is_none() => {
                        state.seat.seat = Some(registry.bind(name, version.min(SEAT), qh, ()));
                    }
                    "xdg_wm_base" if state.wm_base.is_none() => {
                        state.wm_base = Some(registry.bind(name, version.min(WM_BASE), qh, ()));
                    }
                    "wp_cursor_shape_manager_v1" if state.cursor_shape.is_none() => {
                        let manager: WpCursorShapeManagerV1 = registry.bind(name, 1, qh, ());
                        if let Some(pointer) = &state.seat.pointer {
                            state.seat.cursor_device = Some(manager.get_pointer(pointer, qh, ()));
                        }
                        state.cursor_shape = Some(manager);
                    }
                    "zxdg_output_manager_v1" if state.xdg_outputs.is_none() => {
                        let manager: ZxdgOutputManagerV1 =
                            registry.bind(name, version.min(XDG_OUTPUT), qh, ());
                        for output in &mut state.outputs {
                            if output.xdg.is_none() {
                                output.xdg =
                                    Some(manager.get_xdg_output(&output.output, qh, output.global));
                            }
                        }
                        state.xdg_outputs = Some(manager);
                    }
                    "wl_output" => state.add_output(name, version, registry, qh),
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.retain(|global| global.name != name);
                if let Some(at) = state
                    .outputs
                    .iter()
                    .position(|output| output.global == name)
                {
                    let output = state.outputs.remove(at);
                    if let Some(xdg) = output.xdg {
                        xdg.destroy();
                    }
                    if output.output.version() >= 3 {
                        output.output.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        &name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.output_mut(name) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { transform, .. } => {
                info.sideways = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                // Only the current mode: the others are what it could be.
                if let WEnum::Value(flags) = flags
                    && flags.contains(wl_output::Mode::Current)
                {
                    info.mode = (width, height);
                    info.refresh_mhz = refresh;
                }
            }
            wl_output::Event::Scale { factor } => info.scale = factor.max(1),
            wl_output::Event::Done => info.done = true,
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        &name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::LogicalSize { width, height } = event
            && let Some(info) = state.output_mut(name)
        {
            info.logical_extent = Some((width, height));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        if capabilities.contains(wl_seat::Capability::Pointer) && state.seat.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            if let Some(manager) = &state.cursor_shape {
                state.seat.cursor_device = Some(manager.get_pointer(&pointer, qh, ()));
            }
            state.seat.pointer = Some(pointer);
        }
        if capabilities.contains(wl_seat::Capability::Keyboard) && state.seat.keyboard.is_none() {
            state.seat.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &WlPointer,
        event: wl_pointer::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let seat = &mut state.seat;
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x: x,
                surface_y: y,
            } => {
                let Some(surface) = Self::surface_of(&surface) else {
                    return;
                };
                let seat = &mut state.seat;
                seat.enter_serial = serial;
                seat.pointer_focus = Some(surface);
                seat.pointer_at = (x, y);
                seat.apply_cursor();
                state
                    .pending
                    .push(Event::Pointer(PointerEvent::Enter { surface, x, y }));
            }
            wl_pointer::Event::Leave { .. } => {
                if seat.pointer_focus.take().is_some() {
                    state.pending.push(Event::Pointer(PointerEvent::Leave));
                }
            }
            wl_pointer::Event::Motion {
                surface_x: x,
                surface_y: y,
                ..
            } => {
                seat.pointer_at = (x, y);
                if let Some(surface) = seat.pointer_focus {
                    state
                        .pending
                        .push(Event::Pointer(PointerEvent::Motion { surface, x, y }));
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: pressed,
                ..
            } => {
                let pressed = pressed == WEnum::Value(wl_pointer::ButtonState::Pressed);
                if pressed {
                    seat.input_serial = serial;
                }
                if let Some(surface) = seat.pointer_focus {
                    let (x, y) = seat.pointer_at;
                    state.pending.push(Event::Pointer(PointerEvent::Button {
                        surface,
                        x,
                        y,
                        button,
                        pressed,
                    }));
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let sum = seat.axis.get_or_insert_with(Axis::default);
                if axis == WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    sum.vertical += value;
                } else {
                    sum.horizontal += value;
                }
                // Before version 5 there is no frame to wait for.
                if pointer.version() < 5 {
                    state.flush_axis();
                }
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                let sum = seat.axis.get_or_insert_with(Axis::default);
                if axis == WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    sum.discrete.0 += discrete;
                } else {
                    sum.discrete.1 += discrete;
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let vertical = axis == WEnum::Value(wl_pointer::Axis::VerticalScroll);
                let residue = if vertical {
                    &mut seat.residue.0
                } else {
                    &mut seat.residue.1
                };
                *residue = residue.saturating_add(value120);
                let clicks = *residue / 120;
                *residue -= clicks * 120;
                let sum = seat.axis.get_or_insert_with(Axis::default);
                if vertical {
                    sum.discrete.0 += clicks;
                } else {
                    sum.discrete.1 += clicks;
                }
            }
            wl_pointer::Event::Frame => state.flush_axis(),
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    state.seat.read_keymap(fd, size);
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                if let Some(surface) = Self::surface_of(&surface) {
                    state
                        .pending
                        .push(Event::Keyboard(KeyboardEvent::Enter(surface)));
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                state.pending.push(Event::Keyboard(KeyboardEvent::Leave));
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: key_state,
                ..
            } => {
                let pressed = key_state == WEnum::Value(wl_keyboard::KeyState::Pressed);
                if pressed {
                    state.seat.input_serial = serial;
                }
                state
                    .pending
                    .push(Event::Keyboard(KeyboardEvent::Key { code: key, pressed }));
            }
            wl_keyboard::Event::Modifiers { group, .. } => state.seat.group = group,
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, SurfaceId> for State {
    fn event(
        state: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        &surface: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg.ack_configure(serial);
        let size = match state.surfaces.get(&surface) {
            Some(Surface {
                role: Role::Popup { pending, .. },
                ..
            }) => *pending,
            // Zero is "you choose": the size the window has, or, before it
            // has one, the size it asked for. A window the compositor sized
            // and then let go (floated) keeps its size rather than snapping
            // back.
            Some(Surface {
                role: Role::Toplevel { pending, size, .. },
                configured,
                ..
            }) => {
                let (width, height) = configured.unwrap_or(*size);
                (
                    if pending.0 == 0 { width } else { pending.0 },
                    if pending.1 == 0 { height } else { pending.1 },
                )
            }
            None => return,
        };
        state.configured(surface, size.0, size.1);
    }
}

impl Dispatch<XdgToplevel, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        &surface: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The size the `xdg_surface.configure` that follows commits to.
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if let Some(Surface {
                    role: Role::Toplevel { pending, .. },
                    ..
                }) = state.surfaces.get_mut(&surface)
                {
                    *pending = (nonnegative(width), nonnegative(height));
                }
            }
            xdg_toplevel::Event::Close => state.pending.push(Event::CloseRequested(surface)),
            _ => {}
        }
    }
}

impl Dispatch<XdgPopup, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &XdgPopup,
        event: xdg_popup::Event,
        &surface: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure { width, height, .. } => {
                if let Some(Surface {
                    role: Role::Popup { pending, .. },
                    ..
                }) = state.surfaces.get_mut(&surface)
                {
                    *pending = (nonnegative(width), nonnegative(height));
                }
            }
            xdg_popup::Event::PopupDone => state.pending.push(Event::Closed(surface)),
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        &surface: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.pending.push(Event::Frame(surface));
        }
    }
}

impl Dispatch<WlBuffer, BufferData> for State {
    fn event(
        state: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_buffer::Event::Release = event else {
            return;
        };
        match *data {
            BufferData::Window(surface) => {
                if let Some(slot) = state
                    .surfaces
                    .get_mut(&surface)
                    .and_then(|state| state.slots.iter_mut().find(|slot| slot.buffer == *buffer))
                {
                    slot.busy = false;
                }
            }
            BufferData::Cursor => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        } = event
        {
            state.pending.push(Event::RelativeMotion {
                utime: (u64::from(utime_hi) << 32) | u64::from(utime_lo),
                delta: (dx, dy),
                unaccelerated: (dx_unaccel, dy_unaccel),
            });
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        lock: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let locked = match event {
            zwp_locked_pointer_v1::Event::Locked => true,
            zwp_locked_pointer_v1::Event::Unlocked => false,
            _ => return,
        };
        state.pending.push(Event::Lock {
            lock: lock.clone(),
            locked,
        });
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            ext_data_control_device_v1::Event::DataOffer { id } => ClipboardEvent::DataOffer(id),
            ext_data_control_device_v1::Event::Selection { id } => ClipboardEvent::Selection {
                which: 0,
                offer: id,
            },
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                ClipboardEvent::Selection {
                    which: 1,
                    offer: id,
                }
            }
            ext_data_control_device_v1::Event::Finished => ClipboardEvent::Finished,
            _ => return,
        };
        state.pending.push(Event::Clipboard(event));
    }

    // An offer's types follow the event that makes it.
    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.pending.push(Event::Clipboard(ClipboardEvent::Offer {
                offer: offer.clone(),
                mime: mime_type,
            }));
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            ext_data_control_source_v1::Event::Send { fd, .. } => ClipboardEvent::Send {
                source: source.clone(),
                fd,
            },
            ext_data_control_source_v1::Event::Cancelled => {
                ClipboardEvent::Cancelled(source.clone())
            }
            _ => return,
        };
        state.pending.push(Event::Clipboard(event));
    }
}

/// Objects whose events the host does not need.
macro_rules! quiet {
    ($($interface:ty),* $(,)?) => {$(
        impl Dispatch<$interface, ()> for State {
            fn event(
                _: &mut Self,
                _: &$interface,
                _: <$interface as Proxy>::Event,
                (): &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

quiet!(
    WlCompositor,
    WlShm,
    WlShmPool,
    XdgPositioner,
    ZxdgOutputManagerV1,
    WpCursorShapeManagerV1,
    WpCursorShapeDeviceV1,
    ZwpRelativePointerManagerV1,
    ZwpPointerConstraintsV1,
    ExtDataControlManagerV1,
);

/// The surfaces' `enter`, `leave` and preferred scale: the host draws at
/// buffer scale 1 whatever the screen.
impl Dispatch<WlSurface, SurfaceData> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as Proxy>::Event,
        _: &SurfaceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = "! model
  pc105           Generic 105-key PC

! layout
  us              English (US)
  de              German

! variant
  nodeadkeys      de: German (no dead keys)
  dvorak          us: English (Dvorak)
";

    #[test]
    fn a_keymaps_group_name_is_a_layout_and_variant_of_the_rules() {
        let named = |label| layout_named(RULES, label);
        assert_eq!(named("German"), Some(("de".to_owned(), String::new())));
        assert_eq!(
            named("German (no dead keys)"),
            Some(("de".to_owned(), "nodeadkeys".to_owned()))
        );
        assert_eq!(
            named("English (Dvorak)"),
            Some(("us".to_owned(), "dvorak".to_owned()))
        );
        assert_eq!(named("Generic 105-key PC"), None, "a model is no layout");
        assert_eq!(named("Klingon"), None);
    }

    #[test]
    fn an_outputs_logical_size_is_xdg_outputs_else_the_mode_over_the_scale() {
        let mut output = Output {
            mode: (2560, 1440),
            scale: 2,
            ..Output::default()
        };
        assert_eq!(output.logical_size(), (1280, 720));
        output.sideways = true;
        assert_eq!(output.logical_size(), (720, 1280));
        output.logical_extent = Some((1707, 960));
        assert_eq!(output.logical_size(), (1707, 960));
    }

    #[test]
    fn a_buffers_length_is_four_bytes_a_pixel_and_fits_an_int() {
        assert_eq!(length(2, 3), Some(24));
        assert_eq!(length(0, 3), None);
        assert_eq!(length(65536, 65536), None);
    }
}
