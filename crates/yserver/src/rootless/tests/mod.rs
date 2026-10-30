//! `RecordingHost` and a headless surface backend driven by wire requests,
//! with the loop's order in `tail`.

mod cpu;
mod frames;
mod gpu;
mod readiness;
mod record;
mod shell;

use std::{
    io,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, MutexGuard},
};

use ash::vk;
use yserver_core::{
    backend::{Backend, BackendTopology},
    resources::ROOT_WINDOW,
    rootless::{CoordinatePolicy, RootlessEvent, ToplevelId},
    server::ServerState,
};

use super::{
    Note, Presenter,
    host::{CpuFrame, FrameEvent, FrameSeq, HandoffKind, RootlessHost},
};
use crate::{
    kms::{
        render::{XBackend, store::DrawableId, target::Src},
        vk::device::{VkContext, VkContextOptions},
    },
    surface::{SurfacePlatform, headless_backend, link::ShellLink},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Setup {
    Immediate,
    Withheld,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Ready {
    Immediate,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Deliver {
    Immediate,
    Manual,
    /// Drop the next frame (then `Ready` if `Ready::Immediate`), deliver after.
    DropNext,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Release {
    Immediate,
    Never,
    Manual,
}

/// One host call, in order.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Call {
    Events(Vec<RootlessEvent>),
    /// `submit_cpu`: (id, seq).
    Cpu(ToplevelId, FrameSeq),
    FirstFrame(ToplevelId),
    FrameWanted(ToplevelId),
    SurfaceReleased(ToplevelId),
    /// `surface_resizing`: (id, extent).
    Resizing(ToplevelId, (u32, u32)),
    Rebuilt(ToplevelId),
    /// What the presenter did (its test observer).
    Presenter(Note),
}

pub(super) struct HostLog {
    pub setup: Setup,
    pub ready: Ready,
    pub deliver: Deliver,
    pub release: Release,
    pub handoff: HandoffKind,
    pub calls: Vec<Call>,
    /// Every submitted frame's metadata and a copy of its bytes.
    pub frames: Vec<FrameCopy>,
    /// Frames the host still holds (`Release::Never`/`Manual`).
    pub held: Vec<CpuFrame>,
    /// When set, held frames go here instead of `held` (another thread).
    pub sink: Option<std::sync::mpsc::Sender<CpuFrame>>,
}

/// A submitted `CpuFrame` as it arrived.
#[derive(Clone, Debug)]
pub(super) struct FrameCopy {
    pub id: ToplevelId,
    pub seq: FrameSeq,
    pub backing_gen: u64,
    pub width: u16,
    pub height: u16,
    pub stride: u32,
    pub depth: u8,
    pub pixels: Vec<u8>,
    pub damage: Vec<yserver_core::rootless::Rect>,
}

/// A host that records every call and answers on the link as configured.
pub(super) struct RecordingHost {
    log: Arc<Mutex<HostLog>>,
    link: Arc<ShellLink>,
}

impl RootlessHost for RecordingHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> HandoffKind {
        lock(&self.log).handoff
    }

    /// A `VK_EXT_headless_surface` (the context must enable it).
    fn create_surface(&mut self, _id: ToplevelId, vk: &VkContext) -> io::Result<vk::SurfaceKHR> {
        let loader = ash::ext::headless_surface::Instance::new(&vk.entry, &vk.instance);
        // SAFETY: an instance with the extension enabled (`gpu_or_skip`).
        unsafe {
            loader.create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None)
        }
        .map_err(|e| io::Error::other(format!("headless surface: {e:?}")))
    }

    fn submit_cpu(&mut self, frame: CpuFrame) {
        let mut log = lock(&self.log);
        log.calls.push(Call::Cpu(frame.id, frame.seq));
        log.frames.push(FrameCopy {
            id: frame.id,
            seq: frame.seq,
            backing_gen: frame.backing_gen,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            depth: frame.depth,
            pixels: frame.pixels.clone(),
            damage: frame.damage.clone(),
        });
        let (id, seq) = (frame.id, frame.seq);
        match log.deliver {
            Deliver::Immediate => self.link.post_frame(FrameEvent::Delivered { id, seq }),
            Deliver::Manual => {}
            Deliver::DropNext => {
                log.deliver = Deliver::Immediate;
                self.link.post_frame(FrameEvent::Dropped { id, seq });
            }
        }
        if log.deliver != Deliver::Manual && log.ready == Ready::Immediate {
            self.link.post_frame(FrameEvent::Ready { id });
        }
        match log.release {
            Release::Immediate => self.link.post_frame(FrameEvent::Released {
                id,
                seq,
                pixels: frame.pixels,
            }),
            Release::Never | Release::Manual => match &log.sink {
                Some(sink) => sink.send(frame).expect("sink thread"),
                None => log.held.push(frame),
            },
        }
    }

    fn first_frame(&mut self, id: ToplevelId) {
        lock(&self.log).calls.push(Call::FirstFrame(id));
    }

    fn on_events(&mut self, events: &[RootlessEvent]) {
        let mut log = lock(&self.log);
        log.calls.push(Call::Events(events.to_vec()));
        if log.setup == Setup::Immediate {
            for event in events {
                if let RootlessEvent::Mapped { id, .. } = event {
                    self.link.post_frame(FrameEvent::SetupDone { id: *id });
                }
            }
        }
    }

    /// An on-demand pacing signal: `Ready` at once when `Ready::Immediate`.
    fn frame_wanted(&mut self, id: ToplevelId) {
        let mut log = lock(&self.log);
        log.calls.push(Call::FrameWanted(id));
        if log.ready == Ready::Immediate {
            self.link.post_frame(FrameEvent::Ready { id });
        }
    }

    fn surface_released(&mut self, id: ToplevelId) {
        lock(&self.log).calls.push(Call::SurfaceReleased(id));
    }

    fn surface_resizing(&mut self, id: ToplevelId, extent: (u32, u32)) {
        lock(&self.log).calls.push(Call::Resizing(id, extent));
    }

    fn surface_rebuilt(&mut self, id: ToplevelId) {
        lock(&self.log).calls.push(Call::Rebuilt(id));
    }
}

pub(super) fn lock(log: &Mutex<HostLog>) -> MutexGuard<'_, HostLog> {
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const CLIENT: u32 = 9;
const GC: u32 = 0x0020_0001;

/// A 256x192 headless surface backend, a client, a link.
pub(super) struct Fixture {
    pub b: XBackend<SurfacePlatform>,
    pub state: ServerState,
    pub link: Arc<ShellLink>,
    pub log: Arc<Mutex<HostLog>>,
    peer: UnixStream,
    sequence: u16,
}

impl Fixture {
    /// Rooted: no presenter, rootless off.
    pub fn rooted_or_skip() -> Option<Self> {
        Self::rooted_with(VkContextOptions {
            wants_present: false,
            surface_extension: None,
            wants_kms_interop: false,
        })
    }

    /// Rootless with `VulkanSurface` windows on headless surfaces.
    pub fn gpu_or_skip(configure: impl FnOnce(&mut HostLog)) -> Option<Self> {
        let mut f = Self::rooted_with(VkContextOptions {
            wants_present: true,
            surface_extension: Some(ash::ext::headless_surface::NAME),
            wants_kms_interop: false,
        })?;
        f.install(|log| {
            log.handoff = HandoffKind::VulkanSurface;
            configure(log);
        });
        Some(f)
    }

    fn rooted_with(options: VkContextOptions) -> Option<Self> {
        let vk = match VkContext::new_with_options(options) {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping: no Vk: {e:?}");
                return None;
            }
        };
        let mut b = headless_backend(vk, (256, 192), None).expect("headless backend");
        let link = Arc::new(ShellLink::default());
        b.pstate.link = Some(Arc::clone(&link));
        Some(Self::with_backend(b, link))
    }

    /// Over `b`, whose link is `link`; no presenter yet.
    pub fn with_backend(mut b: XBackend<SurfacePlatform>, link: Arc<ShellLink>) -> Self {
        let mut state = BackendTopology::from_backend(&mut b).into_server_state();
        yserver_core::backend::install_backend_root_bindings(&mut state, &b);
        let peer = add_client(&mut state);
        let log = Arc::new(Mutex::new(HostLog {
            setup: Setup::Immediate,
            ready: Ready::Immediate,
            deliver: Deliver::Immediate,
            release: Release::Immediate,
            handoff: HandoffKind::Cpu,
            calls: Vec::new(),
            frames: Vec::new(),
            held: Vec::new(),
            sink: None,
        }));
        let mut f = Self {
            b,
            state,
            link,
            log,
            peer,
            sequence: 0,
        };
        // CreateGC(GC, root, no values); `fill` sets the foreground.
        f.request(
            55,
            0,
            &[GC.to_le_bytes(), ROOT_WINDOW.0.to_le_bytes(), [0; 4]].concat(),
        );
        f
    }

    /// Rootless, with a `RecordingHost` set up by `configure`.
    pub fn rootless_or_skip(configure: impl FnOnce(&mut HostLog)) -> Option<Self> {
        let mut f = Self::rooted_or_skip()?;
        f.install(configure);
        Some(f)
    }

    /// Rootless with `host` (no recording host).
    pub fn with_host_or_skip(
        host: impl FnOnce(Arc<ShellLink>) -> Box<dyn RootlessHost>,
    ) -> Option<Self> {
        let mut f = Self::rooted_or_skip()?;
        let host = host(Arc::clone(&f.link));
        super::start(&mut f.state, &mut f.b, host, CoordinatePolicy::Opaque)
            .expect("rootless start");
        Some(f)
    }

    fn install(&mut self, configure: impl FnOnce(&mut HostLog)) {
        self.install_with(CoordinatePolicy::Opaque, configure);
    }

    /// A `RecordingHost` under `policy`.
    pub fn install_with(&mut self, policy: CoordinatePolicy, configure: impl FnOnce(&mut HostLog)) {
        let f = self;
        configure(&mut lock(&f.log));
        let host = RecordingHost {
            log: Arc::clone(&f.log),
            link: Arc::clone(&f.link),
        };
        super::start(&mut f.state, &mut f.b, Box::new(host), policy).expect("rootless start");
        let log = Arc::clone(&f.log);
        f.presenter().observer = Some(Box::new(move |note| {
            lock(&log).calls.push(Call::Presenter(note));
        }));
    }

    /// The context's validation errors; 0 asserted by every GPU test.
    pub fn assert_validation_clean(&self) {
        use crate::kms::render::scene::OutputSink as _;
        let vk = self.b.platform.gpu().vk().expect("vk");
        if vk.validation_enabled() {
            assert_eq!(vk.validation_errors(), 0, "validation errors");
        } else {
            eprintln!("validation was off");
        }
    }

    pub fn presenter(&mut self) -> &mut Presenter {
        self.b
            .pstate
            .rootless
            .as_mut()
            .expect("presenter installed")
    }

    /// The presenter's own deadline, and the whole backend's.
    pub fn wakeups(
        &self,
        now: std::time::Instant,
    ) -> (Option<std::time::Instant>, Option<std::time::Instant>) {
        let presenter = self
            .b
            .pstate
            .rootless
            .as_ref()
            .and_then(|p| p.next_wakeup(&self.b.store, now));
        (presenter, self.b.next_wakeup())
    }

    /// Whether `window`'s CPU readback is in flight.
    pub fn in_flight(&mut self, window: u32) -> bool {
        let id = self.id(window);
        matches!(&self.presenter().windows[&id].path, super::Path::Cpu(cpu) if cpu.in_flight())
    }

    pub fn calls(&self) -> Vec<Call> {
        lock(&self.log).calls.clone()
    }

    /// One loop iteration's platform drain and tail (`run.rs`: the
    /// `PlatformEvent` dispatch, then reconcile, then `maybe_composite`).
    pub fn tail(&mut self) {
        self.b.on_platform_event(&mut self.state);
        yserver_core::rootless::reconcile_and_dispatch(&mut self.state, &mut self.b);
        self.b.maybe_composite().expect("maybe_composite");
    }

    pub fn request(&mut self, opcode: u8, data: u8, body: &[u8]) {
        use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};
        assert_eq!(body.len() % 4, 0, "padded body");
        self.sequence += 1;
        yserver_core::core_loop::process_request::process_request(
            &mut self.state,
            &mut self.b as &mut dyn Backend,
            ClientId(CLIENT),
            SequenceNumber(self.sequence),
            RequestHeader {
                opcode,
                data,
                length_units: u32::try_from(1 + body.len() / 4).expect("length"),
            },
            body,
            None,
        )
        .expect("process_request");
        self.assert_no_error(opcode);
    }

    fn assert_no_error(&mut self, opcode: u8) {
        use std::io::Read;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 65536];
        while let Ok(n) = self.peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        let mut rest = bytes.as_slice();
        while rest.len() >= 32 {
            assert_ne!(rest[0], 0, "request {opcode}: X error {}", rest[1]);
            let len = match rest[0] {
                1 => 32 + 4 * u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize,
                _ => 32,
            };
            rest = &rest[len.min(rest.len())..];
        }
    }

    /// CreateWindow (InputOutput, CopyFromParent depth and visual), no values.
    pub fn create_window(&mut self, wid: u32, parent: u32, rect: (i16, i16, u16, u16), bw: u16) {
        let body = [
            wid.to_le_bytes().as_slice(),
            &parent.to_le_bytes(),
            &rect.0.to_le_bytes(),
            &rect.1.to_le_bytes(),
            &rect.2.to_le_bytes(),
            &rect.3.to_le_bytes(),
            &bw.to_le_bytes(),
            &1u16.to_le_bytes(),
            &0u32.to_le_bytes(),
            &0u32.to_le_bytes(),
        ]
        .concat();
        self.request(1, 0, &body);
    }

    pub fn map(&mut self, window: u32) {
        self.request(8, 0, &window.to_le_bytes());
    }

    pub fn unmap(&mut self, window: u32) {
        self.request(10, 0, &window.to_le_bytes());
    }

    /// ConfigureWindow width and height.
    pub fn resize(&mut self, window: u32, width: u16, height: u16) {
        let body = [
            window.to_le_bytes().as_slice(),
            &0x000cu16.to_le_bytes(),
            &[0, 0],
            &u32::from(width).to_le_bytes(),
            &u32::from(height).to_le_bytes(),
        ]
        .concat();
        self.request(12, 0, &body);
    }

    /// PolyFillRectangle `rect` of `drawable` with `pixel`.
    pub fn fill(&mut self, drawable: u32, pixel: u32, rect: (i16, i16, u16, u16)) {
        self.request(
            56,
            0,
            &[GC.to_le_bytes(), 4u32.to_le_bytes(), pixel.to_le_bytes()].concat(),
        );
        let body = [
            drawable.to_le_bytes().as_slice(),
            &GC.to_le_bytes(),
            &rect.0.to_le_bytes(),
            &rect.1.to_le_bytes(),
            &rect.2.to_le_bytes(),
            &rect.3.to_le_bytes(),
        ]
        .concat();
        self.request(70, 0, &body);
    }

    /// Tail until the host received `n` frames (2 s at most).
    pub fn frames_until(&mut self, n: usize) -> Vec<FrameCopy> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            self.tail();
            let frames = lock(&self.log).frames.clone();
            if frames.len() >= n {
                return frames;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} of {n} frames: {:?}",
                frames.len(),
                self.calls()
            );
            std::thread::yield_now();
        }
    }

    /// `window`'s redirect backing.
    pub fn backing(&self, window: u32) -> DrawableId {
        let pixmap = self.b.core.host_window_to_backing[&self.host_xid(window)];
        self.b
            .store
            .lookup(pixmap.as_raw())
            .expect("backing in the store")
    }

    /// `get_image` of the whole backing: the oracle for frame bytes.
    pub fn full_readback(&mut self, window: u32) -> Vec<u8> {
        use crate::kms::render::scene::OutputSink as _;
        let backing = self.backing(window);
        let extent = self.b.store.get(backing).expect("backing").storage.extent;
        self.b
            .engine
            .get_image(
                &mut self.b.store,
                self.b.platform.gpu_mut(),
                Src::server_internal(backing),
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                },
                32,
            )
            .expect("get_image")
    }

    /// The backing's unacknowledged presentation damage.
    pub fn store_damage(&self, window: u32) -> Vec<vk::Rect2D> {
        self.b
            .store
            .peek_presentation_damage(self.backing(window))
            .map(|s| s.region.rects().to_vec())
            .unwrap_or_default()
    }

    /// The live id of top-level `window`.
    pub fn id(&self, window: u32) -> ToplevelId {
        self.state
            .rootless
            .as_ref()
            .and_then(|r| r.toplevel_id(yserver_protocol::x11::ResourceId(window)))
            .expect("a mapped top-level")
    }

    /// `window`'s backend handle.
    pub fn host_xid(&self, window: u32) -> u32 {
        self.state
            .resources
            .window(yserver_protocol::x11::ResourceId(window))
            .and_then(|w| w.host_xid)
            .expect("backend window")
            .as_raw()
    }
}

fn add_client(state: &mut ServerState) -> UnixStream {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::atomic::AtomicU16,
    };
    use yserver_core::{server::ClientState, transport::Transport};
    let (peer, writer) = UnixStream::pair().expect("socket pair");
    writer.set_nonblocking(true).expect("nonblocking");
    peer.set_nonblocking(true).expect("nonblocking");
    state.clients.insert(
        CLIENT,
        ClientState {
            writer: Arc::new(Mutex::new(Transport::Unix(writer))),
            byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            gone: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
            internal: false,
            peer: yserver_core::peer::PeerIdentity::unknown_unix(),
        },
    );
    peer
}
