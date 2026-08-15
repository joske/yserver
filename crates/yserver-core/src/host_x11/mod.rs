mod pump;
mod request;
mod sequence_map;
mod trait_impl;

pub use pump::{
    HostConfigureEvent, HostEvent, HostExposeEvent, HostKeyEvent, HostPointerEvent,
    HostSubwindowConfig, PointerEventKind, PointerPosition,
};

use std::{
    collections::{HashMap, VecDeque},
    io::{self, ErrorKind, Read, Write},
    os::fd::{AsRawFd, RawFd},
};

use log::debug;
use yserver_protocol::x11::ResourceId;

use crate::backend::{OriginContext, PixmapHandle, WindowHandle};

use crate::core_loop::client_reader::wait_readable;

use pump::{HostSetup, HostStream, connect_to_host, decode_host_event, read_setup_reply};
use sequence_map::SequenceMap;

/// Outcome of a single `drain_host_socket` pass.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HostSocketStatus {
    /// Read drained the kernel buffer; nothing more to read right now.
    /// Frames decoded out of `read_buffer` have been classified.
    WouldBlock,
    /// Host closed the connection. The core's host-X11 poll-source arm
    /// posts `Message::Shutdown` in response.
    Eof,
}

/// Pull a single complete X11 frame off the front of `buf` if one is
/// fully buffered; otherwise return `None`. Reply (`header[0] == 1`)
/// and GenericEvent (`header[0] == 35`) frames carry an extra-payload
/// length in `header[4..8]` (in 4-byte units); everything else is a
/// fixed 32-byte frame.
fn try_extract_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 32 {
        return None;
    }
    let header_byte = buf[0];
    let total = match header_byte {
        1 | 35 => 32 + (u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize) * 4,
        _ => 32,
    };
    if buf.len() < total {
        return None;
    }
    Some(buf.drain(..total).collect())
}

pub(crate) const POINTER_EVENT_MASK: u32 = 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040 // PointerMotion
    | 0x0000_8000; // Exposure

pub(crate) const SUBWINDOW_EVENT_MASK: u32 = 0x0000_8000; // Exposure

pub(super) enum ResponseMatch {
    Return,
    Buffer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HostError {
    pub code: u8,
    pub sequence: u16,
    pub sequence_full: u64,
    pub major_opcode: u8,
    pub minor_opcode: u16,
    pub bad_value: u32,
}

impl HostError {
    pub(super) fn from_response(response: &HostResponse) -> Option<Self> {
        if response.bytes[0] != 0 {
            return None;
        }
        Some(Self {
            code: response.bytes[1],
            sequence: response.sequence,
            sequence_full: response.sequence_full,
            bad_value: read_u32(&response.bytes[4..8]),
            minor_opcode: read_u16(&response.bytes[8..10]),
            major_opcode: response.bytes[10],
        })
    }

    pub(super) fn into_io_error(self, request_name: &str) -> io::Error {
        io::Error::other(format!(
            "host {request_name} failed (error {} major={} minor={} bad=0x{:x} seq={} seq_full={})",
            self.code,
            self.major_opcode,
            self.minor_opcode,
            self.bad_value,
            self.sequence,
            self.sequence_full,
        ))
    }
}

struct HostRenderInfo {
    opcode: u8,
    fmt_a1: u32,
    fmt_a8: u32,
    fmt_rgb24: u32,
    fmt_argb32: u32,
}

struct HostXkbInfo {
    opcode: u8,
    first_event: u8,
    first_error: u8,
}

pub struct HostX11Backend {
    stream: HostStream,
    window_id: u32,
    gc_id: u32,
    current_foreground: u32,
    current_background: u32,
    current_clip: HostClipState,
    current_fill: HostFillState,
    /// Phase 6.2 additive scope: cached values on the host's shared GC
    /// for the GC attributes that yserver-core now forwards. Setting
    /// these on first use means we don't re-issue identical ChangeGC's.
    /// Using `None` for the initial value means "host default, don't
    /// know exact byte" — the first non-default request will issue a
    /// ChangeGC unconditionally.
    current_function: Option<u8>,
    current_plane_mask: Option<u32>,
    current_line_width: Option<u16>,
    current_line_style: Option<u8>,
    current_cap_style: Option<u8>,
    current_join_style: Option<u8>,
    current_fill_rule: Option<u8>,
    current_subwindow_mode: Option<u8>,
    current_graphics_exposures: Option<bool>,
    current_dash_offset: Option<i16>,
    current_dashes: Option<Vec<u8>>,
    current_arc_mode: Option<u8>,
    active_origin: Option<OriginContext>,
    sequence: u16,
    next_seq_full: u64,
    next_xid_counter: u32,
    render: Option<HostRenderInfo>,
    xkb: Option<HostXkbInfo>,
    /// Major opcode of the host's SHAPE extension, cached on init. `None`
    /// means the host doesn't advertise SHAPE — forwarders become no-ops.
    shape_opcode: Option<u8>,
    /// Major opcode of the host's XFIXES extension. Used so far only by
    /// `ChangeCursorByName`; other XFIXES requests are still served locally.
    xfixes_opcode: Option<u8>,
    /// Major opcode of the host's COMPOSITE extension. Used to forward
    /// `Composite::NameWindowPixmap` so that compositors (picom, mutter)
    /// see actual host backing-store contents through our nested layer.
    /// `None` means the host doesn't advertise COMPOSITE — clients then
    /// receive `BadAlloc` for `NameWindowPixmap`.
    composite_opcode: Option<u8>,
    /// Host XID of the host root visual. Pushed into `ResourceTable` so
    /// that core CreateWindow forwarding for our `ROOT_VISUAL` resolves
    /// to a real host visual.
    root_visual_xid: u32,
    /// Host XID of an ARGB (32-bit TrueColor) visual on the host, if
    /// one was advertised at setup. `None` means we can't honour
    /// `ARGB_VISUAL` for top-level CreateWindow on this host.
    argb_visual_xid: Option<u32>,
    /// Host XID of a colormap allocated for `argb_visual_xid` during
    /// init. Required by `CreateWindow` whenever the child visual is
    /// not `CopyFromParent`.
    argb_colormap_xid: Option<u32>,
    /// Replies/errors decoded from the host socket and not yet
    /// consumed by `wait_for_reply`. Single-threaded core (F2): the
    /// core thread is the only producer (via `drain_host_socket`) and
    /// the only consumer (via `wait_for_reply` from inside
    /// `process_request`), so no Arc/Mutex/Condvar.
    pending_replies: SequenceMap<HostResponse>,
    /// Sliding-window cache of host errors that arrived without a
    /// waiter in `pending_replies`. After F2 errors land in
    /// `pending_replies` directly and this is rarely populated; kept
    /// around for the init-phase synchronous fallback.
    pending_errors: SequenceMap<HostError>,
    /// Origin context per outstanding request, looked up when a reply
    /// or error frame arrives so we can attribute it back to the
    /// nested client that issued the host request.
    pending_origins: SequenceMap<OriginContext>,
    /// Decoded async events the host has pushed but the core hasn't
    /// fanned out yet. F2 reentrancy invariant: `drain_host_socket`
    /// only enqueues; `dispatch_pending_host_events` (run at
    /// outer-loop boundary) drains and fans out. This makes nested
    /// `wait_for_reply` recursion impossible — a host method called
    /// inside fanout cannot re-enter event dispatch.
    pending_events: VecDeque<HostEvent>,
    /// Partial-frame buffer for non-blocking reads from the host
    /// socket. `drain_host_socket` keeps reading until `EAGAIN`,
    /// extracting whole X11 frames as they become complete.
    read_buffer: Vec<u8>,
    /// Set once the host socket has signalled EOF; `drain_host_socket`
    /// returns `Eof` from then on so the core posts `Shutdown`.
    socket_eof: bool,
    host_event_masks: HashMap<u32, u32>,
    /// `host_xid → ResourceId` lookup table consulted by host event
    /// fanout (`pointer_event_fanout_to_state`,
    /// `expose_event_fanout_to_state`). After F2 this is a plain
    /// HashMap — only the core thread touches it.
    xid_map: HostXidMap,
    // GCs cached per pixmap depth. The default `gc_id` is bound to a depth-24
    // drawable so PutImage onto pixmaps with a different depth (e.g. depth-8
    // alpha masks for RENDER) would BadMatch. We lazily create one GC per
    // depth using the target drawable as the screen-and-depth reference.
    depth_gcs: HashMap<u8, u32>,
    // Depth of every host drawable we own, populated at create_pixmap and
    // create_subwindow time. Drain on free_pixmap / destroy_subwindow so
    // it doesn't grow unbounded across xts cycles. Read by draw helpers
    // to pick a GC with a matching depth — using the depth-24 `gc_id`
    // against a depth-32 destination would BadMatch on the host and
    // silently drop the request (xts bucket 1 root cause).
    host_drawable_depths: HashMap<u32, u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostClipRectangles {
    pub ordering: u8,
    pub x_origin: i16,
    pub y_origin: i16,
    pub rectangles: Vec<u8>,
}

/// Tracks what clip-state the host shared GC currently has, so we don't
/// re-issue identical `SetClipRectangles` / `ChangeGC(clip-mask)` calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum HostClipState {
    /// `clip-mask = None` — no clipping, draw everywhere.
    None,
    /// Clip to a list of rectangles set via `SetClipRectangles`.
    Rectangles(HostClipRectangles),
    /// Clip to the 1-bits of a depth-1 host pixmap, shifted by
    /// `(x_origin, y_origin)`. Used by wmaker for window-decoration
    /// symbols (close-button "X" etc.).
    Pixmap {
        host_pixmap: u32,
        x_origin: i16,
        y_origin: i16,
    },
}

/// Tracks the fill-style on the host shared GC so we don't re-issue
/// identical `ChangeGC(fill-style+tile)` calls. e16 paints popup
/// backgrounds via Tiled fill; the fill handlers must flip to Tiled
/// before the draw and back to Solid after, otherwise other clients'
/// later draws would inherit the tile pixmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostFillState {
    Solid,
    Tiled {
        host_pixmap: u32,
        x_origin: i16,
        y_origin: i16,
    },
}

/// Visual / depth / colormap selector for [`HostX11Backend::create_subwindow`].
/// `CopyFromParent` is the historical path — depth=0, visual=0, no
/// colormap value — used when the requested child visual matches the
/// host container's visual. `Explicit` carries the host xids needed to
/// honour ARGB top-levels: the host requires both a real visual id and
/// a colormap whose visual matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSubwindowVisual {
    CopyFromParent,
    /// Preserve the nested window depth for non-host backends that do
    /// not need an upstream visual/colormap translation. Host X11
    /// request encoding treats this like CopyFromParent; KMS backends
    /// use the carried depth directly.
    DepthOnly {
        depth: u8,
    },
    Explicit {
        depth: u8,
        visual_xid: u32,
        colormap_xid: u32,
    },
}

impl HostSubwindowVisual {
    pub(super) fn depth(self) -> u8 {
        match self {
            Self::CopyFromParent | Self::DepthOnly { .. } => 0,
            Self::Explicit { depth, .. } => depth,
        }
    }

    pub(super) fn visual_xid(self) -> u32 {
        match self {
            Self::CopyFromParent | Self::DepthOnly { .. } => 0,
            Self::Explicit { visual_xid, .. } => visual_xid,
        }
    }
}

/// Host-XID → nested ResourceId lookup. F2 demoted this from
/// `Arc<Mutex<HashMap>>` to a plain `HashMap` — only the core thread
/// reads or writes it, so the lock is unnecessary.
pub type HostXidMap = HashMap<u32, ResourceId>;

impl HostX11Backend {
    pub fn open_from_env(width: u16, height: u16) -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let setup = read_setup_reply(&mut stream)?;
        let window_id = setup.resource_id_base;
        let gc_id = setup.resource_id_base + 1;
        let font_id = setup.resource_id_base + 2;
        create_window(&mut stream, &setup, window_id, width, height)?;
        open_font(&mut stream, font_id, b"fixed")?;
        create_gc(
            &mut stream,
            window_id,
            gc_id,
            setup.black_pixel,
            setup.white_pixel,
            font_id,
        )?;
        map_window(&mut stream, window_id)?;
        stream.flush()?;

        // Map the host container window to ynest's ROOT_WINDOW so that
        // Expose / pointer events on the container area route through
        // ROOT_WINDOW in `expose_event_fanout_to_state` /
        // `pointer_event_fanout_to_state`.
        let mut xid_map: HostXidMap = HashMap::new();
        xid_map.insert(window_id, crate::resources::ROOT_WINDOW);

        let mut this = Self {
            stream,
            window_id,
            gc_id,
            current_foreground: setup.black_pixel,
            current_background: setup.white_pixel,
            current_clip: HostClipState::None,
            current_fill: HostFillState::Solid,
            current_function: None,
            current_plane_mask: None,
            current_line_width: None,
            current_line_style: None,
            current_cap_style: None,
            current_join_style: None,
            current_fill_rule: None,
            current_subwindow_mode: None,
            current_graphics_exposures: None,
            current_dash_offset: None,
            current_dashes: None,
            current_arc_mode: None,
            active_origin: None,
            sequence: 5,
            next_seq_full: 5,
            next_xid_counter: setup.resource_id_base + 3,
            render: None,
            xkb: None,
            shape_opcode: None,
            xfixes_opcode: None,
            composite_opcode: None,
            pending_replies: SequenceMap::new(65_536),
            pending_errors: SequenceMap::new(65_536),
            pending_origins: SequenceMap::new(65_536),
            pending_events: VecDeque::new(),
            read_buffer: Vec::new(),
            socket_eof: false,
            host_event_masks: HashMap::new(),
            xid_map,
            depth_gcs: HashMap::new(),
            host_drawable_depths: HashMap::new(),
            root_visual_xid: setup.root_visual,
            argb_visual_xid: setup.argb_visual,
            argb_colormap_xid: None,
        };
        // Init paths run synchronously on a still-blocking stream:
        // `read_until_response` drains until each QueryExtension reply
        // arrives. Once init completes we set the stream non-blocking
        // and the core's `drain_host_socket` takes over.
        this.render = this.init_render().ok();
        this.xkb = this.init_xkb().ok();
        this.shape_opcode = this.query_extension_opcode(b"SHAPE").ok().flatten();
        if this.shape_opcode.is_none() {
            log::info!("host SHAPE extension absent — top-level shape forwarding disabled");
        }
        this.xfixes_opcode = this.query_extension_opcode(b"XFIXES").ok().flatten();
        if this.xfixes_opcode.is_none() {
            log::info!("host XFIXES extension absent — cursor-by-name forwarding disabled");
        }
        this.composite_opcode = this.query_extension_opcode(b"Composite").ok().flatten();
        if this.composite_opcode.is_none() {
            log::info!("host COMPOSITE extension absent — NameWindowPixmap will return BadAlloc");
        }
        if let Some(argb_visual) = this.argb_visual_xid {
            match this.create_argb_colormap(setup.root, argb_visual) {
                Ok(xid) => this.argb_colormap_xid = Some(xid),
                Err(err) => {
                    log::warn!(
                        "could not allocate host ARGB colormap (visual=0x{argb_visual:x}): {err}; \
                         ARGB CreateWindow will fall back to CopyFromParent"
                    );
                }
            }
        } else {
            log::info!("host advertises no depth-32 TrueColor visual — ARGB CreateWindow disabled");
        }
        // F2: keep the stream *blocking* so `write_all` for large
        // host requests (PutImage, server-issued ChangeGC, etc.)
        // doesn't trip on EAGAIN. Reads use MSG_DONTWAIT per call
        // in `drain_host_socket` so the core thread can drain
        // without ever blocking on a stuck host. `wait_for_reply`
        // alternates `wait_readable` (poll(2)) with
        // `drain_host_socket` to wait for a specific reply without
        // blocking the entire core.
        Ok(this)
    }

    /// Raw fd of the host X11 connection. The core registers this with
    /// its mio poller (using a unique backend poll-source token) so readiness wakes the core
    /// to call `drain_host_socket`.
    pub(super) fn host_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Host XIDs the upper layer pushes into the visual / colormap
    /// tables in `ResourceTable`. `argb_*` are `None` when the host
    /// has no depth-32 TrueColor visual.
    pub fn root_visual_xid(&self) -> u32 {
        self.root_visual_xid
    }

    pub fn argb_visual_xid(&self) -> Option<u32> {
        self.argb_visual_xid
    }

    pub fn argb_colormap_xid(&self) -> Option<u32> {
        self.argb_colormap_xid
    }

    /// Allocate a host colormap for our ARGB visual via `XCreateColormap(
    /// alloc=None, mid, root, visual)`. Sent fire-and-forget — host errors
    /// (visual not depth-32, etc.) become async and are absorbed silently;
    /// the resulting xid is still returned but if the host failed, later
    /// CreateWindow attempts using it will surface a host BadColor / BadValue
    /// (also absorbed). This is acceptable here — the alternative is a
    /// blocking sync round-trip during HostX11Backend init.
    fn create_argb_colormap(&mut self, host_root: u32, argb_visual: u32) -> io::Result<u32> {
        let cmap_id = self.next_xid();
        let mut out = Vec::with_capacity(16);
        out.push(78); // CreateColormap opcode
        out.push(0); // alloc = None
        write_u16(&mut out, 4); // length = 4 words
        write_u32(&mut out, cmap_id);
        write_u32(&mut out, host_root);
        write_u32(&mut out, argb_visual);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(cmap_id)
    }

    /// Major opcode of the host's COMPOSITE extension, or `None` if the
    /// host didn't advertise it at startup. The nested COMPOSITE handler
    /// uses this to gate `NameWindowPixmap` forwarding.
    #[must_use]
    pub fn composite_opcode(&self) -> Option<u8> {
        self.composite_opcode
    }

    /// Forward `Composite::NameWindowPixmap(window, pixmap)` to the host.
    /// Caller is responsible for validating `host_window` is a redirected
    /// host top-level. Allocates a fresh host pixmap XID and returns it.
    /// No reply is generated by the host.
    pub fn name_window_pixmap(&mut self, host_window: WindowHandle) -> io::Result<PixmapHandle> {
        let Some(major) = self.composite_opcode else {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "host COMPOSITE extension not available",
            ));
        };
        let host_pixmap = self.next_xid();
        // Wire layout: opcode(1) minor(1) length(2 = 3) window(4) pixmap(4)
        let mut out = [0u8; 12];
        out[0] = major;
        out[1] = yserver_protocol::x11::composite::NAME_WINDOW_PIXMAP;
        out[2..4].copy_from_slice(&3u16.to_le_bytes());
        out[4..8].copy_from_slice(&host_window.as_raw().to_le_bytes());
        out[8..12].copy_from_slice(&host_pixmap.to_le_bytes());
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(PixmapHandle::from_raw_panicking(host_pixmap))
    }

    pub(super) fn next_xid(&mut self) -> u32 {
        let xid = self.next_xid_counter;
        self.next_xid_counter = self.next_xid_counter.wrapping_add(1);
        xid
    }

    pub(super) fn update_host_event_mask(
        &mut self,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> u32 {
        let mut current = self.host_event_masks.get(&host_xid).copied().unwrap_or(0);
        if enabled {
            current |= mask;
        } else {
            current &= !mask;
        }
        if current == 0 {
            self.host_event_masks.remove(&host_xid);
            0
        } else {
            self.host_event_masks.insert(host_xid, current);
            current
        }
    }

    /// Phase 6.3 Step 4: write the `event-mask` value-list bit on the
    /// host child via the merged main connection. Caller has already
    /// updated the registry through [`update_host_event_mask`]; this
    /// is the wire-side commit. Pre-Step-4 the per-pump pump-handle
    /// `register_*` issued the same write on a *separate* socket —
    /// folding it onto the main stream is the "Big Flip" core.
    pub(super) fn write_event_mask(&mut self, host_xid: u32, event_mask: u32) -> io::Result<()> {
        // ChangeWindowAttributes — value-mask bit 11 (event-mask).
        let mut out = Vec::with_capacity(16);
        out.push(2); // ChangeWindowAttributes opcode
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, 1 << 11);
        write_u32(&mut out, event_mask);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(())
    }

    /// `host_xid → ResourceId` lookup table. F2: plain HashMap, no
    /// locking — `pointer_event_fanout_to_state` /
    /// `expose_event_fanout_to_state` borrow it immutably through the
    /// backend.
    pub(super) fn xid_map(&self) -> &HostXidMap {
        &self.xid_map
    }

    /// Register a host top-level window so its pointer / expose
    /// events route to `nested_id` in `dispatch_pending_host_events`.
    pub(super) fn register_top_level(
        &mut self,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        // Insert into the map *before* the wire write so any pointer
        // events arriving on this xid after the host commits the new
        // event-mask immediately resolve to the right ResourceId.
        self.xid_map.insert(host_xid, nested_id);
        let combined = self.update_host_event_mask(host_xid, POINTER_EVENT_MASK, true);
        self.write_event_mask(host_xid, combined)
    }

    /// Counterpart of [`register_top_level`] for sub-windows —
    /// registers `Exposure` only so pointer events bubble up to the
    /// top-level ancestor (X11 propagation rule), keeping dispatch
    /// on the top-level.
    pub(super) fn register_subwindow(
        &mut self,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        let combined = self.update_host_event_mask(host_xid, SUBWINDOW_EVENT_MASK, true);
        self.write_event_mask(host_xid, combined)
    }

    /// Drop the `host_xid → ResourceId` mapping at DestroyWindow /
    /// Reparent-out. Prevents stale lookups in fanout helpers.
    pub(super) fn unregister_host_window(&mut self, host_xid: u32) {
        self.xid_map.remove(&host_xid);
        self.host_event_masks.remove(&host_xid);
    }

    #[cfg(test)]
    fn host_event_mask(&self, host_xid: u32) -> u32 {
        self.host_event_masks.get(&host_xid).copied().unwrap_or(0)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn set_active_origin(&mut self, origin: Option<OriginContext>) {
        self.active_origin = origin;
    }

    pub(super) fn with_active_origin<R>(
        &mut self,
        origin: Option<OriginContext>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let previous = self.active_origin;
        self.active_origin = origin;
        let result = f(self);
        self.active_origin = previous;
        result
    }

    fn record_origin(&mut self, seq_full: u64) {
        if let Some(origin) = self.active_origin {
            self.pending_origins.insert(seq_full, origin);
        }
    }

    fn take_origin_for_sequence(&mut self, target_full: u64) -> Option<OriginContext> {
        self.pending_origins.take(target_full)
    }

    pub(super) fn issue_sequence(&mut self) -> (u16, u64) {
        let wire = self.sequence;
        let full = self.next_seq_full;
        self.record_origin(full);
        self.sequence = self.sequence.wrapping_add(1);
        self.next_seq_full = self.next_seq_full.wrapping_add(1);
        (wire, full)
    }

    pub(super) fn advance_sequence(&mut self) {
        let _ = self.issue_sequence();
    }

    fn promote_sequence(&self, wire: u16) -> u64 {
        promote_seq_from_atomic(self.next_seq_full, wire)
    }

    pub(super) fn read_host_response(&mut self) -> io::Result<HostResponse> {
        let mut response = read_response(&mut self.stream)?;
        response.sequence_full = self.promote_sequence(response.sequence);
        Ok(response)
    }

    pub(super) fn take_buffered_reply(&mut self, target_full: u64) -> Option<HostResponse> {
        let response = self.pending_replies.take(target_full)?;
        self.take_origin_for_sequence(target_full);
        Some(response)
    }

    /// Stash an init-time response into the pending tables (called
    /// from the synchronous `read_until_response` path used during
    /// `open_from_env`, before the socket is set non-blocking).
    pub(super) fn stash_or_log_response(&mut self, response: HostResponse) {
        if response.bytes[0] == 0 {
            let origin = self.take_origin_for_sequence(response.sequence_full);
            let error = HostError::from_response(&response).expect("host error response");
            debug!(
                "host async error dropped: code={} major={} minor={} seq={} seq_full={} origin={origin:?}",
                error.code,
                error.major_opcode,
                error.minor_opcode,
                error.sequence,
                error.sequence_full,
            );
            self.pending_errors.insert(error.sequence_full, error);
            return;
        }
        self.take_origin_for_sequence(response.sequence_full);
        self.pending_replies
            .insert(response.sequence_full, response);
    }

    /// Block until `target_full` lands in `pending_replies`. F2: the
    /// core thread is the only reader of the host socket, so this
    /// drives the socket directly. `wait_readable` blocks on `poll(2)`
    /// when the kernel buffer is empty; `drain_host_socket` then
    /// extracts whatever frames have arrived. Spontaneous host events
    /// land in `pending_events` (drained later at the outer-loop
    /// boundary, never recursively from inside this call — that's the
    /// reentrancy invariant documented on `HostX11Backend`).
    pub(super) fn wait_for_reply(
        &mut self,
        target_full: u64,
    ) -> io::Result<Result<Vec<u8>, HostError>> {
        // Fast path: a previous `drain_host_socket` already produced
        // the response.
        if let Some(response) = self.take_buffered_reply(target_full) {
            return Ok(decode_reply_or_error(response));
        }
        // Init-phase fallback: an error was queued in `pending_errors`
        // by `stash_or_log_response` during the synchronous init.
        if let Some(error) = self.pending_errors.take(target_full) {
            self.take_origin_for_sequence(target_full);
            return Ok(Err(error));
        }
        // Drain whatever's currently buffered (non-blocking via
        // MSG_DONTWAIT), poll for more, repeat. Reentrancy invariant:
        // `drain_host_socket` only enqueues — it does not fan out
        // events. A host method called inside fanout cannot
        // recursively re-dispatch.
        loop {
            match self.drain_host_socket()? {
                HostSocketStatus::WouldBlock => {}
                HostSocketStatus::Eof => {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "host connection closed before reply arrived",
                    ));
                }
            }
            if let Some(response) = self.take_buffered_reply(target_full) {
                return Ok(decode_reply_or_error(response));
            }
            wait_readable(self.stream.as_raw_fd())?;
        }
    }

    /// Synchronous read-buffer drain used by the init path before the
    /// socket goes non-blocking. After init, `drain_host_socket` is
    /// the canonical reader.
    pub(super) fn read_until_response<F>(&mut self, mut matcher: F) -> io::Result<HostResponse>
    where
        F: FnMut(&HostResponse) -> ResponseMatch,
    {
        loop {
            let response = self.read_host_response()?;
            match matcher(&response) {
                ResponseMatch::Return => {
                    self.take_origin_for_sequence(response.sequence_full);
                    return Ok(response);
                }
                ResponseMatch::Buffer => self.stash_or_log_response(response),
            }
        }
    }

    /// Read whatever bytes are currently available on the host socket
    /// and decode complete X11 frames out of `read_buffer`. Replies
    /// and errors land in `pending_replies`; events land in
    /// `pending_events` (fanned out later at the outer-loop boundary).
    ///
    /// The stream stays in blocking mode so `write_all` for large
    /// host requests doesn't trip on EAGAIN; reads here go through
    /// `recv(MSG_DONTWAIT)` which bypasses the socket's blocking
    /// flag for this one call. Returns `WouldBlock` when the kernel
    /// buffer is drained, `Eof` when the host closed the connection.
    pub(super) fn drain_host_socket(&mut self) -> io::Result<HostSocketStatus> {
        if self.socket_eof {
            return Ok(HostSocketStatus::Eof);
        }
        let fd = self.stream.as_raw_fd();
        let mut tmp = [0u8; 4096];
        loop {
            // SAFETY: `recv(2)` with MSG_DONTWAIT on a connected
            // Unix socket; `tmp` is a valid mutable slice.
            let n = unsafe {
                libc::recv(
                    fd,
                    tmp.as_mut_ptr().cast::<libc::c_void>(),
                    tmp.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                self.socket_eof = true;
                self.classify_buffered_frames();
                return Ok(HostSocketStatus::Eof);
            }
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    ErrorKind::WouldBlock => {
                        self.classify_buffered_frames();
                        return Ok(HostSocketStatus::WouldBlock);
                    }
                    ErrorKind::Interrupted => continue,
                    _ => return Err(err),
                }
            }
            #[allow(clippy::cast_sign_loss)]
            let n = n as usize;
            self.read_buffer.extend_from_slice(&tmp[..n]);
        }
    }

    fn classify_buffered_frames(&mut self) {
        while let Some(frame) = try_extract_frame(&mut self.read_buffer) {
            self.classify_frame(frame);
        }
    }

    fn classify_frame(&mut self, frame: Vec<u8>) {
        let header_byte = frame[0];
        match header_byte {
            0 => {
                // Error
                let sequence = u16::from_le_bytes([frame[2], frame[3]]);
                let sequence_full = self.promote_sequence(sequence);
                let response = HostResponse {
                    sequence,
                    sequence_full,
                    bytes: frame,
                };
                if let Some(error) = HostError::from_response(&response) {
                    let origin = self.take_origin_for_sequence(sequence_full);
                    debug!(
                        "host async error: code={} major={} minor={} seq={} seq_full={} origin={origin:?}",
                        error.code,
                        error.major_opcode,
                        error.minor_opcode,
                        error.sequence,
                        error.sequence_full,
                    );
                }
                self.pending_replies.insert(sequence_full, response);
            }
            1 => {
                // Reply
                let sequence = u16::from_le_bytes([frame[2], frame[3]]);
                let sequence_full = self.promote_sequence(sequence);
                let response = HostResponse {
                    sequence,
                    sequence_full,
                    bytes: frame,
                };
                self.pending_replies.insert(sequence_full, response);
            }
            35 => {
                // GenericEvent: not currently surfaced.
            }
            _ => {
                if let Ok(header) = <[u8; 32]>::try_from(&frame[..32])
                    && let Some(event) = decode_host_event(&header)
                {
                    self.pending_events.push_back(event);
                }
            }
        }
    }

    /// Pop the next decoded host event for fanout. The core's
    /// dispatcher loop calls this at the outer-loop boundary (after
    /// each `Message` and each non-host-X11 token arm) so a host
    /// request issued mid-fanout cannot recursively re-dispatch.
    pub(super) fn pop_pending_host_event(&mut self) -> Option<HostEvent> {
        self.pending_events.pop_front()
    }

    /// Enqueue a host event from outside the wire decoder (XTEST `FakeInput`).
    /// The event is fanned out at the next outer-loop boundary by
    /// `dispatch_pending_host_events`, exactly like real host events.
    pub(crate) fn push_pending_host_event(&mut self, event: HostEvent) {
        self.pending_events.push_back(event);
    }

    pub(super) fn host_socket_eof(&self) -> bool {
        self.socket_eof
    }

    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    pub fn render_opcode(&self) -> Option<u8> {
        self.render.as_ref().map(|r| r.opcode)
    }

    pub fn xkb_opcode(&self) -> Option<u8> {
        self.xkb.as_ref().map(|r| r.opcode)
    }

    pub fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        self.xkb
            .as_ref()
            .map(|r| (r.opcode, r.first_event, r.first_error))
    }

    pub fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        let r = self.render.as_ref()?;
        match ynest_fmt {
            1 => Some(r.fmt_a1),
            2 => Some(r.fmt_a8),
            3 => Some(r.fmt_rgb24),
            4 => Some(r.fmt_argb32),
            _ => None,
        }
    }

    fn init_render(&mut self) -> io::Result<HostRenderInfo> {
        let ext_name = b"RENDER";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let (ext_seq, ext_seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(98u8);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, ext_name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(ext_name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        debug!(
            "init_render: sent QueryExtension RENDER, expecting seq={}",
            ext_seq
        );

        let resp = self.read_until_response(|response| {
            debug!(
                "init_render: got response byte0={} seq={}",
                response.bytes[0], response.sequence
            );
            if response.sequence_full == ext_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Err(io::Error::other("host RENDER extension not present"));
        }
        let opcode = resp.bytes[9];
        debug!("init_render: RENDER present, opcode={}", opcode);

        let (fmt_seq, fmt_seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(1); // QueryPictFormats
        write_u16(&mut out, 1);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        debug!(
            "init_render: sent QueryPictFormats, expecting seq={}",
            fmt_seq
        );

        let resp = self.read_until_response(|response| {
            debug!(
                "init_render: got response byte0={} seq={}",
                response.bytes[0], response.sequence
            );
            if response.sequence_full == fmt_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        let info = parse_host_pict_formats(&resp.bytes, opcode)?;
        debug!(
            "init_render: host formats a1=0x{:x} a8=0x{:x} rgb24=0x{:x} argb32=0x{:x}",
            info.fmt_a1, info.fmt_a8, info.fmt_rgb24, info.fmt_argb32
        );
        Ok(info)
    }

    fn init_xkb(&mut self) -> io::Result<HostXkbInfo> {
        let ext_name = b"XKEYBOARD";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let (_ext_seq, ext_seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(98u8);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, ext_name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(ext_name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let resp = self.read_until_response(|response| {
            if response.sequence_full == ext_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Err(io::Error::other("host XKEYBOARD extension not present"));
        }
        let opcode = resp.bytes[9];
        let first_event = resp.bytes[10];
        let first_error = resp.bytes[11];

        // We also need to send UseExtension to the host for XKB to be fully functional.
        let (_use_seq, use_seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0); // UseExtension
        write_u16(&mut out, 2);
        write_u16(&mut out, 1); // want major 1
        write_u16(&mut out, 0); // want minor 0
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let resp = self.read_until_response(|response| {
            if response.sequence_full == use_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Err(io::Error::other("host XKB UseExtension failed"));
        }

        Ok(HostXkbInfo {
            opcode,
            first_event,
            first_error,
        })
    }

    /// Issue `QueryExtension(name)` on the host stream and return the major
    /// opcode if the extension is present. Used for capability probes that
    /// don't need the first-event/first-error fields (`init_render` and
    /// `init_xkb` cache those for their own bookkeeping).
    fn query_extension_opcode(&mut self, name: &[u8]) -> io::Result<Option<u8>> {
        let padded = padded_len(name.len());
        let length_units = 2 + (padded / 4) as u16;
        let (_ext_seq, ext_seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(98u8); // QueryExtension
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        let resp = self.read_until_response(|response| {
            if response.sequence_full == ext_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Ok(None);
        }
        Ok(Some(resp.bytes[9]))
    }
}

fn create_window(
    stream: &mut HostStream,
    setup: &HostSetup,
    window_id: u32,
    width: u16,
    height: u16,
) -> io::Result<()> {
    // Value-mask: bg-pixel (bit 1) | bit-gravity (bit 4) | event-mask (bit 11).
    // bit-gravity = NorthWest (1) so a host-side resize preserves the NW pixels.
    // Without this the gravity defaults to Forget and the host server is free
    // to clear the entire container on resize, which paints over every visible
    // subwindow and leaves the desktop blank until the apps redraw.
    let value_mask: u32 = (1 << 1) | (1 << 4) | (1 << 11);
    // length = 3 fixed words + 1 word per value bit (3 values). 3 + 3 = 6
    // fixed; add 4-word CreateWindow header → 10 total length units.
    let mut out = Vec::new();
    out.push(1);
    out.push(setup.root_depth);
    write_u16(&mut out, 11);
    write_u32(&mut out, window_id);
    write_u32(&mut out, setup.root);
    write_i16(&mut out, 80);
    write_i16(&mut out, 80);
    write_u16(&mut out, width);
    write_u16(&mut out, height);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);
    write_u32(&mut out, setup.root_visual);
    write_u32(&mut out, value_mask);
    write_u32(&mut out, setup.white_pixel); // bg-pixel
    write_u32(&mut out, 1); // bit-gravity = NorthWest
    // Phase 6.3 Step 4: container holds the *union* of every kind of
    // event we care about so the merged dispatcher sees them all on
    // one connection — KeyPress / KeyRelease (per-client kb fanout),
    // ButtonPress / ButtonRelease / EnterWindow / LeaveWindow /
    // PointerMotion (POINTER_EVENT_MASK), Exposure (root background
    // repaint), StructureNotify (container resize → RANDR fanout).
    let event_mask: u32 = CONTAINER_EVENT_MASK;
    write_u32(&mut out, event_mask);
    stream.write_all(&out)
}

/// Combined event-mask the host container window selects via
/// `CreateWindow`'s value-list. Phase 6.3 Step 4 unifies the masks
/// the deleted `HostInputPump`s used to select on three separate
/// connections — see the comment in [`create_window`].
const CONTAINER_EVENT_MASK: u32 = 0x0000_0001 // KeyPress
    | 0x0000_0002 // KeyRelease
    | 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040 // PointerMotion
    | 0x0000_8000 // Exposure
    | 0x0002_0000; // StructureNotify

fn create_gc(
    stream: &mut HostStream,
    drawable: u32,
    gc_id: u32,
    foreground: u32,
    background: u32,
    font_id: u32,
) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(55);
    out.push(0);
    write_u16(&mut out, 7);
    write_u32(&mut out, gc_id);
    write_u32(&mut out, drawable);
    write_u32(&mut out, (1 << 2) | (1 << 3) | (1 << 14));
    write_u32(&mut out, foreground);
    write_u32(&mut out, background);
    write_u32(&mut out, font_id);
    stream.write_all(&out)
}

fn open_font(stream: &mut HostStream, font_id: u32, name: &[u8]) -> io::Result<()> {
    let padded_name_len = padded_len(name.len());
    let length_units = 3 + u16::try_from(padded_name_len / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font name is too long"))?;

    let mut out = Vec::new();
    out.push(45);
    out.push(0);
    write_u16(&mut out, length_units);
    write_u32(&mut out, font_id);
    write_u16(
        &mut out,
        u16::try_from(name.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font name is too long"))?,
    );
    write_u16(&mut out, 0);
    out.extend_from_slice(name);
    out.resize(12 + padded_name_len, 0);
    stream.write_all(&out)
}

fn map_window(stream: &mut HostStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(8);
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, window_id);
    stream.write_all(&out)
}

pub(super) fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_i16(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(super) fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn write_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn padded_len(len: usize) -> usize {
    (len + 3) & !3
}

pub(super) fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn parse_host_pict_formats(bytes: &[u8], opcode: u8) -> io::Result<HostRenderInfo> {
    if bytes.len() < 32 {
        return Err(io::Error::other("QueryPictFormats reply too short"));
    }
    let num_formats = read_u32(&bytes[8..12]) as usize;
    let mut fmt_a1 = 0u32;
    let mut fmt_a8 = 0u32;
    let mut fmt_rgb24 = 0u32;
    let mut fmt_argb32 = 0u32;
    for i in 0..num_formats {
        let base = 32 + i * 28;
        if base + 28 > bytes.len() {
            break;
        }
        let id = read_u32(&bytes[base..base + 4]);
        let type_ = bytes[base + 4];
        let depth = bytes[base + 5];
        let alpha_shift = read_u16(&bytes[base + 20..base + 22]);
        let alpha_mask = read_u16(&bytes[base + 22..base + 24]);
        let red_shift = read_u16(&bytes[base + 8..base + 10]);
        let red_mask = read_u16(&bytes[base + 10..base + 12]);
        if type_ == 1 {
            match depth {
                1 if alpha_mask == 1 => fmt_a1 = id,
                8 if alpha_mask == 0xFF && alpha_shift == 0 => fmt_a8 = id,
                24 if red_mask == 0xFF && red_shift == 16 && alpha_mask == 0 => fmt_rgb24 = id,
                32 if alpha_mask == 0xFF && alpha_shift == 24 => fmt_argb32 = id,
                _ => {}
            }
        }
    }
    Ok(HostRenderInfo {
        opcode,
        fmt_a1,
        fmt_a8,
        fmt_rgb24,
        fmt_argb32,
    })
}

pub(super) struct HostResponse {
    sequence: u16,
    sequence_full: u64,
    bytes: Vec<u8>,
}

fn decode_reply_or_error(response: HostResponse) -> Result<Vec<u8>, HostError> {
    if let Some(error) = HostError::from_response(&response) {
        Err(error)
    } else {
        Ok(response.bytes)
    }
}

/// Promote a 16-bit wire sequence to 64-bit using the latest known
/// 64-bit counter. `next_full` is the value the *next* request will
/// take — i.e. one past the most recently issued sequence — so a wire
/// sequence equal to `next_full as u16` should resolve to
/// `next_full - (1 << 16)` because the host can't possibly have replied
/// to a not-yet-issued request.
///
/// The 32k sliding window in `SequenceMap` limits how stale a response
/// can be before it gets dropped on the floor; with 65k seq space we
/// have ~50% headroom before this promotion would alias.
pub(super) fn promote_seq_from_atomic(next_full: u64, wire: u16) -> u64 {
    let base = next_full & !0xffff;
    let mut candidate = base | u64::from(wire);
    if candidate >= next_full {
        candidate = candidate.saturating_sub(1 << 16);
    }
    candidate
}

/// Read whatever-blocking-mode the stream is in: returns one host
/// frame (reply / error / event). Used by the init-time synchronous
/// loop in `read_until_response`. After init the core uses
/// `drain_host_socket` instead.
pub(super) fn read_response(stream: &mut HostStream) -> io::Result<HostResponse> {
    let mut header = [0u8; 32];
    loop {
        stream.read_exact(&mut header)?;
        match header[0] {
            0 | 1 => break,
            35 => {
                // GenericEvent: may have extra data beyond the 32-byte header.
                // Read and discard any extra bytes to keep the stream aligned.
                let extra =
                    u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4;
                log::debug!(
                    "read_response: GenericEvent extra={} seq={}",
                    extra,
                    u16::from_le_bytes([header[2], header[3]])
                );
                if extra > 0 {
                    let mut tail = vec![0u8; extra];
                    stream.read_exact(&mut tail)?;
                }
                continue;
            }
            t => {
                log::debug!(
                    "read_response: skipping event type={} seq={}",
                    t,
                    u16::from_le_bytes([header[2], header[3]])
                );
                continue;
            }
        }
    }
    let sequence = u16::from_le_bytes([header[2], header[3]]);
    let extra = if header[0] == 1 {
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4
    } else {
        0
    };
    let mut bytes = Vec::with_capacity(32 + extra);
    bytes.extend_from_slice(&header);
    if extra > 0 {
        let mut tail = vec![0u8; extra];
        stream.read_exact(&mut tail)?;
        bytes.extend_from_slice(&tail);
    }
    Ok(HostResponse {
        sequence,
        sequence_full: 0,
        bytes,
    })
}

// `HostMessage` / `read_dispatch_message` were removed in F2 — the
// dispatcher thread is gone; `drain_host_socket` is the canonical
// reader and classifies frames inline (see `classify_frame`).

#[cfg(test)]
mod tests {
    use super::{
        HostClipState, HostError, HostFillState, HostResponse, HostStream, HostX11Backend,
        HostXidMap, SequenceMap, promote_seq_from_atomic, read_u16, try_extract_frame,
    };
    use crate::backend::OriginContext;
    use std::{
        collections::{HashMap, VecDeque},
        os::unix::net::UnixStream,
    };
    use yserver_protocol::x11::ClientId;

    fn dummy_backend() -> HostX11Backend {
        let (stream, _peer) = UnixStream::pair().expect("unix stream pair");
        let stream = HostStream::Unix(stream);
        HostX11Backend {
            stream,
            window_id: 1,
            gc_id: 2,
            current_foreground: 0,
            current_background: 0,
            current_clip: HostClipState::None,
            current_fill: HostFillState::Solid,
            current_function: None,
            current_plane_mask: None,
            current_line_width: None,
            current_line_style: None,
            current_cap_style: None,
            current_join_style: None,
            current_fill_rule: None,
            current_subwindow_mode: None,
            current_graphics_exposures: None,
            current_dash_offset: None,
            current_dashes: None,
            current_arc_mode: None,
            active_origin: None,
            sequence: 0xfffe,
            next_seq_full: 0x1_fffe,
            next_xid_counter: 3,
            render: None,
            xkb: None,
            shape_opcode: None,
            xfixes_opcode: None,
            composite_opcode: None,
            root_visual_xid: 0,
            argb_visual_xid: None,
            argb_colormap_xid: None,
            pending_replies: SequenceMap::new(65_536),
            pending_errors: SequenceMap::new(65_536),
            pending_origins: SequenceMap::new(65_536),
            pending_events: VecDeque::new(),
            read_buffer: Vec::new(),
            socket_eof: false,
            host_event_masks: HashMap::new(),
            xid_map: HostXidMap::new(),
            depth_gcs: HashMap::new(),
            host_drawable_depths: HashMap::new(),
        }
    }

    #[test]
    fn promote_sequence_handles_wrap_against_latest_full_counter() {
        let mut backend = dummy_backend();
        let (wire0, full0) = backend.issue_sequence();
        let (wire1, full1) = backend.issue_sequence();
        assert_eq!((wire0, full0), (0xfffe, 0x1_fffe));
        assert_eq!((wire1, full1), (0xffff, 0x1_ffff));
        assert_eq!(backend.promote_sequence(0xffff), 0x1_ffff);
        assert_eq!(backend.promote_sequence(0x0000), 0x1_0000);
        assert_eq!(backend.promote_sequence(0xfffd), 0x1_fffd);
    }

    #[test]
    fn issue_sequence_records_and_consumes_origin_by_full_sequence() {
        let mut backend = dummy_backend();
        let origin = OriginContext {
            client_id: ClientId(7),
            nested_seq: 9,
            opcode: 12,
        };
        backend.set_active_origin(Some(origin));
        let (_wire, full) = backend.issue_sequence();
        assert_eq!(backend.take_origin_for_sequence(full), Some(origin));
        assert_eq!(backend.take_origin_for_sequence(full), None);
    }

    #[test]
    fn async_error_logging_path_consumes_origin_and_buffers_error() {
        let mut backend = dummy_backend();
        let origin = OriginContext {
            client_id: ClientId(3),
            nested_seq: 4,
            opcode: 5,
        };
        backend.set_active_origin(Some(origin));
        let (_wire, full) = backend.issue_sequence();
        let mut bytes = vec![0; 32];
        bytes[0] = 0;
        bytes[1] = 3;
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[11] = 42;
        backend.stash_or_log_response(HostResponse {
            sequence: 1,
            sequence_full: full,
            bytes,
        });
        // Errors land in pending_errors via the init-path stash;
        // pending_replies stays empty for error frames.
        assert_eq!(backend.pending_replies.len(), 0);
        assert_eq!(backend.pending_errors.len(), 1);
        // The origin was consumed by stash_or_log_response.
        assert!(backend.take_origin_for_sequence(full).is_none());
        assert_eq!(read_u16(&[1, 0]), 1);
    }

    #[test]
    fn wait_for_reply_decodes_buffered_host_error() {
        let mut backend = dummy_backend();
        let full = 0x1_1234;
        let mut bytes = vec![0; 32];
        bytes[0] = 0;
        bytes[1] = 8;
        bytes[4..8].copy_from_slice(&0x00ab_cdefu32.to_le_bytes());
        bytes[8..10].copy_from_slice(&0x1234u16.to_le_bytes());
        bytes[10] = 62;
        backend.pending_replies.insert(
            full,
            HostResponse {
                sequence: 0x1234,
                sequence_full: full,
                bytes,
            },
        );

        let error = backend
            .wait_for_reply(full)
            .expect("wait_for_reply io result")
            .expect_err("host error result");

        assert_eq!(
            error,
            HostError {
                code: 8,
                sequence: 0x1234,
                sequence_full: full,
                major_opcode: 62,
                minor_opcode: 0x1234,
                bad_value: 0x00ab_cdef,
            }
        );
    }

    #[test]
    fn update_host_event_mask_tracks_registry() {
        let mut backend = dummy_backend();
        assert_eq!(backend.update_host_event_mask(0x1234, 0x4, true), 0x4);
        assert_eq!(backend.host_event_mask(0x1234), 0x4);
        assert_eq!(backend.update_host_event_mask(0x1234, 0x8000, true), 0x8004);
        assert_eq!(backend.host_event_mask(0x1234), 0x8004);
        assert_eq!(backend.update_host_event_mask(0x1234, 0x4, false), 0x8000);
        assert_eq!(backend.host_event_mask(0x1234), 0x8000);
        assert_eq!(backend.update_host_event_mask(0x1234, 0x8000, false), 0);
        assert_eq!(backend.host_event_mask(0x1234), 0);
    }

    /// F2: `try_extract_frame` peels one complete X11 frame off a
    /// non-blocking read buffer and leaves any partial trailer in
    /// place. Reply frames carry an extra-payload length in
    /// `header[4..8]` (in 4-byte units); errors and plain events are
    /// fixed-32-byte.
    #[test]
    fn try_extract_frame_handles_reply_with_extra_payload() {
        // Reply with 4 extra words = 16 extra bytes → 48-byte frame.
        let mut buf = vec![0u8; 48 + 32];
        buf[0] = 1;
        buf[4..8].copy_from_slice(&4u32.to_le_bytes());
        buf[48] = 2; // Next frame start (32-byte event)
        let frame = try_extract_frame(&mut buf).expect("first frame extracted");
        assert_eq!(frame.len(), 48);
        assert_eq!(frame[0], 1);
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 2);
    }

    #[test]
    fn try_extract_frame_returns_none_on_partial_frame() {
        // Reply header claims 8 extra bytes but only 16 buffered.
        let mut buf = vec![0u8; 16];
        buf[0] = 1;
        buf[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(try_extract_frame(&mut buf).is_none());
        // Fewer than 32 bytes — also None.
        let mut tiny = vec![1u8; 8];
        assert!(try_extract_frame(&mut tiny).is_none());
    }

    /// F2 reentrancy invariant: `drain_host_socket` only enqueues —
    /// it never fans events out. Pushing a synthetic Expose frame
    /// through the read buffer should land in `pending_events`, not
    /// trigger any state mutation.
    #[test]
    fn drain_classifies_event_into_pending_events_queue() {
        let mut backend = dummy_backend();
        // Expose event = 12, fixed 32 bytes.
        let mut frame = vec![0u8; 32];
        frame[0] = 12;
        frame[4..8].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        backend.read_buffer.extend_from_slice(&frame);
        backend.classify_buffered_frames();
        assert_eq!(backend.pending_events.len(), 1);
        assert_eq!(backend.read_buffer.len(), 0);
    }

    /// F2: `pop_pending_host_event` is the only way the core lifts a
    /// decoded event out of the backend; calling it on an empty
    /// queue returns None (no fanout work to do).
    #[test]
    fn pop_pending_host_event_returns_none_when_empty() {
        let mut backend = dummy_backend();
        assert!(backend.pop_pending_host_event().is_none());
    }

    /// Sequence promotion edge cases: ancient seq, recent wrap, and
    /// the boundary where the host's wire seq matches the just-issued
    /// sequence (which should resolve to the previous block, never the
    /// not-yet-issued one).
    #[test]
    fn promote_seq_from_atomic_handles_wrap_and_ancient_seqs() {
        // Most recently issued seq_full = 0x2_0000 → next = 0x2_0001.
        // A wire seq of 0x0000 must resolve to 0x2_0000 (the most
        // recent), not 0x2_0000 + 0 (which would be the next request).
        assert_eq!(promote_seq_from_atomic(0x2_0001, 0x0000), 0x2_0000);

        // Wire seq matching the *next* counter exactly (impossibly
        // late) should saturate one block back so we never claim a
        // not-yet-issued slot.
        assert_eq!(promote_seq_from_atomic(0x2_0001, 0x0001), 0x1_0001);

        // Slightly older wire — still in the previous block when
        // newer ones are also possible. The promotion picks the
        // youngest non-future candidate.
        assert_eq!(promote_seq_from_atomic(0x2_0010, 0x000f), 0x2_000f);
        assert_eq!(promote_seq_from_atomic(0x2_0010, 0x0011), 0x1_0011);

        // First wrap: next=0x1_0000, wire=0xffff → 0x0_ffff (last in
        // initial block).
        assert_eq!(promote_seq_from_atomic(0x1_0000, 0xffff), 0x0_ffff);

        // Saturation guard: zero-base, wire larger than next. Returns 0
        // because saturating_sub clamps. Acceptable — sliding window
        // catches it as "too ancient" and the response is dropped.
        assert_eq!(promote_seq_from_atomic(0, 0x0001), 0);
    }

    #[test]
    fn decode_host_event_translates_key_press_and_release() {
        use crate::host_x11::{HostEvent, pump::decode_host_event};

        let mut press = [0u8; 32];
        press[0] = 2; // KeyPress
        press[1] = 0x39;
        press[4..8].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        press[20..22].copy_from_slice(&100i16.to_le_bytes());
        press[22..24].copy_from_slice(&200i16.to_le_bytes());
        press[24..26].copy_from_slice(&50i16.to_le_bytes());
        press[26..28].copy_from_slice(&60i16.to_le_bytes());
        press[28..30].copy_from_slice(&0x0001u16.to_le_bytes());

        match decode_host_event(&press).expect("KeyPress decodes") {
            HostEvent::Key(key) => {
                assert!(key.pressed);
                assert_eq!(key.keycode, 0x39);
                assert_eq!(key.time, 0x0102_0304);
                assert_eq!(key.root_x, 100);
                assert_eq!(key.root_y, 200);
                assert_eq!(key.event_x, 50);
                assert_eq!(key.event_y, 60);
                assert_eq!(key.state, 0x0001);
            }
            other => panic!("expected HostEvent::Key, got {other:?}"),
        }

        let mut release = press;
        release[0] = 3;
        match decode_host_event(&release).expect("KeyRelease decodes") {
            HostEvent::Key(key) => {
                assert!(!key.pressed);
                assert_eq!(key.keycode, 0x39);
            }
            other => panic!("expected HostEvent::Key, got {other:?}"),
        }
    }

    #[test]
    fn decode_host_event_strips_synthetic_flag() {
        use crate::host_x11::{HostEvent, pump::decode_host_event};

        let mut synthetic_press = [0u8; 32];
        synthetic_press[0] = 2 | 0x80;
        synthetic_press[1] = 0x39;
        let event = decode_host_event(&synthetic_press).expect("synthetic key decodes");
        assert!(matches!(event, HostEvent::Key(_)));
    }

    #[test]
    fn decode_host_event_drops_uninteresting_event_types() {
        use crate::host_x11::pump::decode_host_event;

        for unhandled in [11u8, 13, 14, 15, 16, 18, 19, 20, 21, 23, 34] {
            let mut header = [0u8; 32];
            header[0] = unhandled;
            assert!(
                decode_host_event(&header).is_none(),
                "event type {unhandled} should decode to None",
            );
        }
    }

    /// F2: `register_top_level` inserts host_xid → nested_id into the
    /// plain `xid_map` field. Pre-F2 this needed Mutex locking; now
    /// the core is the only writer.
    #[test]
    fn register_top_level_populates_xid_map() {
        use yserver_protocol::x11::ResourceId;
        let mut backend = dummy_backend();
        let nested_id = ResourceId(0x100);
        // The wire side flushes through the dummy stream's peer; the
        // peer side just discards. We only assert the in-memory map.
        let _ = backend.register_top_level(nested_id, 0xabcd_1234);
        let map = HostX11Backend::xid_map(&backend);
        assert_eq!(map.get(&0xabcd_1234).copied(), Some(nested_id));
    }

    /// `SequenceMap` len helper is `#[cfg(test)]` only — surface it
    /// through a trivial test so the compiler doesn't dead-code-strip
    /// it when these tests run.
    #[test]
    fn sequence_map_len_is_test_visible() {
        let mut map: SequenceMap<u32> = SequenceMap::new(8);
        assert_eq!(map.len(), 0);
        map.insert(1, 1);
        assert_eq!(map.len(), 1);
    }
}
