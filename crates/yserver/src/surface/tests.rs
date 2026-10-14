//! The surface platform: sink behaviour without a GPU, the headless backend
//! drawing on lavapipe, and a `VK_EXT_headless_surface` swapchain driven
//! through real compose → copy → present → retire cycles.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use yserver_core::{
    backend::{Backend, BackendTopology, CursorHandle, WindowHandle},
    core_loop::{HostInputEvent, handle_host_input},
    host_x11::HostSubwindowVisual,
    server::ServerState,
};

use super::{
    SurfacePlatform,
    clock::FrameClock,
    headless_backend,
    input::{
        devices::{announcements, keyboard_source, pointer_source},
        keymap::kvk,
        pointer::{RawPointer, RawScroll, ScrollPhase, Scroller, motion},
        translate::{KeyKind, RawKey, Translator},
    },
    link::{ShellEvent, ShellLink},
    surface_backend,
    swapchain::{NativeSurface, SurfaceOutput},
    ui::{CursorImage, ShellUi},
};
use crate::kms::{
    render::{
        Platform, XBackend,
        gpu::GpuCore,
        scene::{CursorPlaneSink, FrameTargetError, OutputRoute, OutputSink},
    },
    vk::device::{VkContext, VkContextOptions},
};

#[test]
fn surface_sink_without_surfaces_has_no_slots_and_no_cursor_plane() {
    let platform = SurfacePlatform::new(GpuCore::for_tests(), (640, 480), Vec::new());
    assert_eq!(platform.fb_size(), (640, 480));
    assert!(platform.output_layouts().is_empty());
    assert_eq!(platform.route(0), OutputRoute::None);
    assert!(platform.slot_count(0).is_none());
    assert!(matches!(
        platform.check_frame_target(0),
        Err(FrameTargetError::NoSlot)
    ));
    assert!(
        !platform.fits(0, 16, 16),
        "no surface, no window-system cursor"
    );
}

fn headless_or_skip() -> Option<XBackend<SurfacePlatform>> {
    let vk = match VkContext::new_with_options(VkContextOptions {
        wants_present: false,
        surface_extension: None,
        wants_kms_interop: false,
    }) {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            return None;
        }
    };
    Some(headless_backend(vk, (64, 48), None).expect("headless backend"))
}

/// Root, an 8x8 window at (4, 4), mapped and realized.
fn create_window(b: &mut XBackend<SurfacePlatform>) -> u32 {
    let root = WindowHandle::from_raw(1).expect("root");
    let window = b
        .create_subwindow(
            None,
            root,
            4,
            4,
            8,
            8,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("create_subwindow");
    let xid = window.as_raw();
    b.map_subwindow(None, xid).expect("map");
    b.realize_window_storage(None, xid).expect("realize");
    // The core's stacking sync, which these tests have no core for.
    b.core.top_level_order.push(xid);
    xid
}

fn fill(b: &mut XBackend<SurfacePlatform>, xid: u32, pixel: u32, rect: (i16, i16, u16, u16)) {
    let bytes = [rect.0.to_le_bytes(), rect.1.to_le_bytes()]
        .concat()
        .into_iter()
        .chain(rect.2.to_le_bytes())
        .chain(rect.3.to_le_bytes())
        .collect::<Vec<u8>>();
    b.poly_fill_rectangle(None, xid, pixel, &bytes)
        .expect("poly_fill_rectangle");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn headless_backend_advertises_no_glx_dpms_or_dri3() {
    let Some(b) = headless_or_skip() else {
        return;
    };
    assert!(!b.glx_available());
    assert!(!b.dpms_capable());
    assert_eq!(b.dri3_capabilities().version, (0, 0));
    assert!(!b.supports_dmabuf_export());
    assert_eq!(b.fb_dimensions(), (64, 48));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn headless_backend_fills_a_window_and_reads_it_back() {
    let Some(mut b) = headless_or_skip() else {
        return;
    };
    let xid = create_window(&mut b);
    fill(&mut b, xid, 0x00FF_0000, (2, 2, 4, 4));
    b.maybe_composite().expect("compose over no outputs");

    let reply = b
        .get_image(None, xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("reply");
    let pixels = &reply[32..];
    let at = |x: usize, y: usize| &pixels[(y * 8 + x) * 4..(y * 8 + x) * 4 + 3];
    assert_eq!(at(3, 3), [0x00, 0x00, 0xFF], "inside the fill is red");
    assert_ne!(at(0, 0), [0x00, 0x00, 0xFF], "outside the fill is not");
}

/// A backend with one 64x48 headless-surface output, its clock and link.
struct SurfaceFixture {
    b: XBackend<SurfacePlatform>,
    clock: Arc<FrameClock>,
    link: Arc<ShellLink>,
    state: ServerState,
}

fn surface_or_skip() -> Option<SurfaceFixture> {
    let native = NativeSurface::headless();
    let vk = match native.create_context() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no presenting Vk: {e}");
            return None;
        }
    };
    let link = Arc::new(ShellLink::default());
    let clock = Arc::new(FrameClock::new(Arc::clone(&link)));
    let extent = vk::Extent2D {
        width: 64,
        height: 48,
    };
    let output = SurfaceOutput::new(Arc::clone(&vk), &native, extent, Arc::clone(&clock))
        .expect("headless surface output");
    let mut b = surface_backend(vk, (64, 48), None, vec![output]).expect("surface backend");
    b.pstate.link = Some(Arc::clone(&link));
    let state = BackendTopology::from_backend(&mut b).into_server_state();
    Some(SurfaceFixture {
        b,
        clock,
        link,
        state,
    })
}

impl SurfaceFixture {
    fn output(&self) -> &SurfaceOutput {
        &self.b.platform.outputs[0]
    }

    /// Tick the clock until the pending frame's copy finished, then let the
    /// core loop's platform-event handler retire it.
    fn retire(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ust = 0;
        while self.output().has_pending() {
            assert!(Instant::now() < deadline, "the frame never retired");
            std::thread::sleep(Duration::from_millis(1));
            ust += 1000;
            if self.clock.tick(ust) {
                self.b.on_platform_event(&mut self.state);
            }
        }
    }

    /// Paint, compose and present one frame.
    fn frame(&mut self, xid: u32, pixel: u32) {
        fill(&mut self.b, xid, pixel, (0, 0, 8, 8));
        self.b.maybe_composite().expect("maybe_composite");
    }

    /// `rect` of the latest on-screen slot, BGRA.
    fn read_slot(&mut self, rect: vk::Rect2D) -> Vec<u8> {
        let image = self.output().on_screen_image().expect("a frame on screen");
        self.b.read_image_region(image, rect).expect("slot read")
    }
}

fn rect(x: i32, y: i32, width: u32, height: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D { width, height },
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_frame_runs_record_acquire_copy_present_then_retires_on_a_tick() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    f.b.platform.outputs[0].trace.clear();
    f.frame(xid, 0x00FF_0000);
    assert_eq!(f.output().frames_presented(), 1);
    assert!(
        f.output().has_pending(),
        "no tick yet: the frame is pending"
    );
    f.retire();
    assert_eq!(
        f.output().trace,
        ["record", "acquire", "copy-submit", "present", "retire"]
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_slots_rotate_and_report_their_buffer_age() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    let mut presented = Vec::new();
    for (frame, pixel) in [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0x00FF_FF00]
        .into_iter()
        .enumerate()
    {
        let token = f.b.platform.outputs[0]
            .acquire_slot(0)
            .expect("a free slot");
        if frame >= 2 {
            assert!(
                token.last_present_generation.is_some(),
                "a reused slot knows what it last presented"
            );
        }
        presented.push(token.bo_idx);
        f.frame(xid, pixel);
        f.retire();
        assert_eq!(f.output().frames_presented(), frame as u64 + 1);
    }
    assert!(
        presented.windows(2).all(|pair| pair[0] != pair[1]),
        "a frame never composes into the slot on screen: {presented:?}"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_on_screen_slot_holds_the_composed_pixels() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    f.frame(xid, 0x00FF_0000);
    f.retire();
    let window = rect(4, 4, 8, 8);
    let slot = f.read_slot(window);
    assert!(
        slot.chunks_exact(4).all(|px| px[..3] == [0x00, 0x00, 0xFF]),
        "the slot shows the red window"
    );
    let root = SurfacePlatform::read_on_screen_region(&mut f.b, window).expect("root read");
    assert_eq!(slot, root, "a root read equals the presented slot");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_rebuilds_an_out_of_date_swapchain_before_the_next_present() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    f.frame(xid, 0x00FF_0000);
    f.retire();
    let old = f.output().swapchain_handle();
    f.b.platform.outputs[0].note_out_of_date();
    f.frame(xid, 0x0000_FF00);
    assert_eq!(f.output().frames_presented(), 2, "presented after rebuild");
    assert_ne!(f.output().swapchain_handle(), old, "a new swapchain");
    f.retire();
    let slot = f.read_slot(rect(4, 4, 8, 8));
    assert!(slot.chunks_exact(4).all(|px| px[..3] == [0x00, 0xFF, 0x00]));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_hidden_window_composes_nothing_and_repaints_when_shown() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    f.frame(xid, 0x00FF_0000);
    f.retire();
    f.link.post(ShellEvent::Visible(false));
    f.b.on_platform_event(&mut f.state);
    f.frame(xid, 0x0000_FF00);
    assert_eq!(f.output().frames_presented(), 1, "nothing presents hidden");
    assert!(!f.output().has_pending());
    assert!(
        SurfacePlatform::next_wakeup(&f.b).is_none_or(|t| t > Instant::now()),
        "a hidden surface does not spin the core loop"
    );
    f.link.post(ShellEvent::Visible(true));
    f.b.on_platform_event(&mut f.state);
    f.b.maybe_composite().expect("maybe_composite");
    assert_eq!(f.output().frames_presented(), 2, "shown again: a new frame");
    f.retire();
    let slot = f.read_slot(rect(4, 4, 8, 8));
    assert!(slot.chunks_exact(4).all(|px| px[..3] == [0x00, 0xFF, 0x00]));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_present_clock_follows_ticks_and_wakes_an_armed_target_once() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let (_poll, sender, rx) = yserver_core::core_loop::channel().expect("channel");
    f.link.connect(sender);
    let wakes = |rx: &yserver_core::core_loop::CoreReceiver| {
        rx.try_recv_all()
            .filter(|m| matches!(m, yserver_core::core_loop::Message::PlatformEvent))
            .count()
    };
    f.clock.tick(16_000);
    assert_eq!(f.b.present_get_ust_msc(2), (1, 16_000));
    let sample = f.b.present_get_completion_clock(2);
    assert_eq!((sample.msc, sample.ust), (1, 16_000));
    assert_eq!(
        sample.source,
        yserver_core::backend::PresentClockSource::BackendVblank
    );

    assert_eq!(f.b.arm_idle_vblanks(2, &[4, 3]).expect("arm"), 1);
    assert_eq!(
        f.b.arm_present_completion_idle_vblanks(2, &[5])
            .expect("arm"),
        0,
        "already armed for an earlier target"
    );
    f.clock.tick(32_000);
    assert_eq!(wakes(&rx), 0, "msc 2 is before the target");
    f.clock.tick(48_000);
    assert_eq!(wakes(&rx), 1, "msc 3 reaches it");
    f.b.on_platform_event(&mut f.state);
    f.clock.tick(64_000);
    assert_eq!(wakes(&rx), 0, "the target fired once");
    assert_eq!(f.b.present_get_ust_msc(2), (4, 64_000));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_display_is_idle_only_without_a_frame_in_flight() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    f.frame(xid, 0x00FF_0000);
    assert!(f.b.present_flip_in_flight(2));
    assert!(!f.b.present_display_idle(2));
    f.retire();
    assert!(!f.b.present_flip_in_flight(2));
    assert!(f.b.present_display_idle(2));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_randr_reports_one_window_output_at_the_screen_size() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    f.clock.set_refresh_mhz(119_880);
    let (outputs, modes) = f.b.randr_outputs_and_modes();
    let [output] = outputs.as_slice() else {
        panic!("one output: {outputs:?}");
    };
    assert_eq!(output.name, "WINDOW-0");
    assert!(output.connected);
    assert_eq!((output.x, output.y), (0, 0));
    assert_eq!((output.width, output.height), (64, 48));
    assert_eq!(output.vrefresh, 120);
    assert_eq!(output.mode_ids.len(), 1);
    let [mode] = modes.as_slice() else {
        panic!("one mode: {modes:?}");
    };
    assert_eq!((mode.width, mode.height, mode.vrefresh), (64, 48, 120));
}

/// A headless backend whose state knows the shell's two input devices, and
/// a client selecting `mask` (core events) on the root.
struct InputFixture {
    b: XBackend<SurfacePlatform>,
    state: ServerState,
    peer: std::os::unix::net::UnixStream,
}

fn input_or_skip(mask: u32) -> Option<InputFixture> {
    let mut b = headless_or_skip()?;
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    for event in announcements(0) {
        handle_host_input(&mut state, &mut b, event);
    }
    let peer = add_client(&mut state, 9, mask);
    Some(InputFixture { b, state, peer })
}

fn add_client(state: &mut ServerState, id: u32, mask: u32) -> std::os::unix::net::UnixStream {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{Mutex, atomic::AtomicU16},
    };
    use yserver_core::{resources::ROOT_WINDOW, server::ClientState, transport::Transport};
    let (peer, writer) = UnixStream::pair().expect("socket pair");
    writer.set_nonblocking(true).expect("nonblocking");
    peer.set_nonblocking(true).expect("nonblocking");
    state.clients.insert(
        id,
        ClientState {
            writer: Arc::new(Mutex::new(Transport::Unix(writer))),
            byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::from([(ROOT_WINDOW, mask)]),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    peer
}

impl InputFixture {
    fn send(&mut self, events: Vec<HostInputEvent>) {
        for event in events {
            handle_host_input(&mut self.state, &mut self.b, event);
        }
    }

    /// `(type, detail, state)` of the core events the client received.
    fn received(&mut self) -> Vec<(u8, u8, u16)> {
        use std::io::Read;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = self.peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        bytes
            .chunks_exact(32)
            .map(|ev| (ev[0] & 0x7f, ev[1], u16::from_le_bytes([ev[28], ev[29]])))
            .collect()
    }
}

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const KEY_MASKS: u32 = 0x1 | 0x2;

fn key(kvk: u16, kind: KeyKind, flags: u64) -> RawKey {
    RawKey {
        kvk,
        kind,
        flags,
        is_repeat: false,
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn shell_devices_are_slaves_of_the_core_devices_without_a_node() {
    use yserver_core::xinput::XiFacetKind;
    let Some(f) = input_or_skip(0) else {
        return;
    };
    let keyboard = f
        .state
        .xi_devices
        .facet(keyboard_source(), XiFacetKind::Keyboard)
        .expect("keyboard slave");
    let pointer = f
        .state
        .xi_devices
        .facet(pointer_source(), XiFacetKind::PointerTouch)
        .expect("pointer slave");
    assert_eq!(f.state.xi_devices.attachment(keyboard), Some(3));
    assert_eq!(f.state.xi_devices.attachment(pointer), Some(2));
    for id in [keyboard, pointer] {
        let device = f.state.xi_devices.device(id).expect("device");
        assert_eq!(device.device_node, None);
        assert!(
            device
                .properties
                .keys()
                .all(|&atom| f.state.atoms.name(atom) != Some("Device Node")),
            "no Device Node property on {id}"
        );
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn translated_keys_reach_a_client_with_their_modifiers() {
    let Some(mut f) = input_or_skip(KEY_MASKS) else {
        return;
    };
    let mut t = Translator::new(false);
    let mut out = Vec::new();
    t.key(key(kvk::kVK_ANSI_A, KeyKind::Down, 0), 1, &mut out);
    t.key(key(kvk::kVK_ANSI_A, KeyKind::Up, 0), 2, &mut out);
    let shift = (1 << 17) | 0x2;
    t.key(
        key(kvk::kVK_Shift, KeyKind::FlagsChanged, shift),
        3,
        &mut out,
    );
    t.key(key(kvk::kVK_ANSI_A, KeyKind::Down, shift), 4, &mut out);
    f.send(out);
    let events = f.received();
    let num_lock = 0x10;
    assert_eq!(
        events,
        [
            (KEY_PRESS, 38, num_lock),
            (KEY_RELEASE, 38, num_lock),
            (KEY_PRESS, 50, num_lock),
            (KEY_PRESS, 38, num_lock | 0x1),
        ],
        "a, then Shift+a with ShiftMask (Num Lock on: Mod2)"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_wheel_click_reaches_a_client_as_button_4() {
    let Some(mut f) = input_or_skip(0x4) else {
        return;
    };
    let mut scroller = Scroller::default();
    let mut out = Vec::new();
    scroller.scroll(
        RawScroll {
            dx: 0.0,
            dy: 1.0,
            precise: false,
            phase: ScrollPhase::None,
            momentum: ScrollPhase::None,
        },
        1,
        &mut out,
    );
    f.send(out);
    let presses: Vec<u8> = f
        .received()
        .into_iter()
        .filter(|&(kind, _, _)| kind == BUTTON_PRESS)
        .map(|(_, detail, _)| detail)
        .collect();
    assert_eq!(presses, [4]);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_held_shell_key_arms_the_core_repeat_until_focus_loss_releases_it() {
    let Some(mut f) = input_or_skip(0) else {
        return;
    };
    let mut t = Translator::new(false);
    let mut out = Vec::new();
    t.key(key(kvk::kVK_ANSI_A, KeyKind::Down, 0), 1, &mut out);
    f.send(out);
    let origin = yserver_core::core_loop::InputOrigin::Physical(keyboard_source());
    assert!(
        f.state
            .key_repeats
            .get(&origin)
            .is_some_and(|repeat| repeat.event.keycode == 38),
        "the core repeats the held key"
    );
    let mut out = Vec::new();
    t.release_all(2, &mut out);
    f.send(out);
    assert!(!f.state.key_repeats.contains_key(&origin), "released");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_lock_sync_toggles_caps_lock_only_when_it_differs() {
    let Some(mut f) = input_or_skip(0) else {
        return;
    };
    let (_poll, sender, rx) = yserver_core::core_loop::channel().expect("channel");
    f.b.set_input_sender(sender);
    let link = Arc::new(ShellLink::default());
    f.b.pstate.link = Some(Arc::clone(&link));
    let sync = |f: &mut InputFixture, caps: bool| -> usize {
        link.post(ShellEvent::LockState { caps });
        f.b.on_platform_event(&mut f.state);
        let events: Vec<HostInputEvent> = rx
            .try_recv_all()
            .filter_map(|m| match m {
                yserver_core::core_loop::Message::HostInput(event) => Some(event),
                _ => None,
            })
            .collect();
        let count = events.len();
        f.send(events);
        count
    };
    assert_eq!(sync(&mut f, false), 0, "both off");
    assert_eq!(sync(&mut f, true), 2, "one Caps press+release");
    assert_eq!(
        f.b.current_xkb_mods().3 & 0x2,
        0x2,
        "X Caps Lock now locked"
    );
    assert_eq!(sync(&mut f, true), 0, "both on");
    assert_eq!(sync(&mut f, false), 2);
    assert_eq!(f.b.current_xkb_mods().3 & 0x2, 0);
}

/// A shown cursor: `(version, hotspot, size)`.
type Shown = (u64, (u16, u16), (u32, u32));

/// A `ShellUi` call, as the cursor tests compare them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UiCall {
    /// The shown cursor, or hidden.
    Cursor(Option<Shown>),
    Warp(i32, i32),
}

#[derive(Default)]
struct RecordingUi(std::sync::Mutex<Vec<UiCall>>);

impl RecordingUi {
    fn take(&self) -> Vec<UiCall> {
        std::mem::take(&mut *self.0.lock().expect("ui calls"))
    }
}

impl ShellUi for RecordingUi {
    fn set_title(&self, _title: &str) {}

    fn quit(&self, _failed: bool) {}

    fn set_cursor(&self, cursor: Option<CursorImage>) {
        let call = cursor.map(|image| {
            assert_eq!(
                image.bgra.len(),
                (image.width * image.height * 4) as usize,
                "BGRA bytes for the whole image"
            );
            (image.version, image.hot, (image.width, image.height))
        });
        self.0.lock().expect("ui calls").push(UiCall::Cursor(call));
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        self.0.lock().expect("ui calls").push(UiCall::Warp(x, y));
    }
}

/// A presenting surface with a recording UI, its first frame retired.
fn cursor_or_skip() -> Option<(SurfaceFixture, Arc<RecordingUi>, u32)> {
    let mut f = surface_or_skip()?;
    let ui = Arc::new(RecordingUi::default());
    f.b.pstate.ui = Some(Arc::clone(&ui) as Arc<dyn ShellUi>);
    let xid = create_window(&mut f.b);
    f.frame(xid, 0x00FF_0000);
    f.retire();
    Some((f, ui, xid))
}

impl SurfaceFixture {
    /// Compose whatever the scene wants, and retire it.
    fn tick(&mut self) {
        self.b.maybe_composite().expect("maybe_composite");
        self.retire();
    }

    /// Whether the core loop would compose now.
    fn compose_due(&self) -> bool {
        SurfacePlatform::next_wakeup(&self.b).is_some_and(|t| t <= Instant::now())
    }

    /// A 16x16 bitmap cursor with hotspot `hot`.
    fn cursor(&mut self, hot: (u16, u16)) -> u32 {
        let pixmap = self.b.create_pixmap(None, 1, 16, 16).expect("pixmap");
        self.b
            .create_cursor(None, pixmap, None, (0xFFFF, 0, 0), (0, 0, 0), hot.0, hot.1)
            .expect("create_cursor")
            .as_raw()
    }

    fn root(&self) -> u32 {
        self.b.core.window_id
    }
}

fn shown(calls: &[UiCall]) -> Vec<Shown> {
    calls
        .iter()
        .map(|call| match call {
            UiCall::Cursor(Some(cursor)) => *cursor,
            other => panic!("expected a shown cursor, got {other:?}"),
        })
        .collect()
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_shows_the_root_cursor_once_at_startup() {
    let Some((mut f, ui, xid)) = cursor_or_skip() else {
        return;
    };
    let [(_, _, (width, height))] = shown(&ui.take())[..] else {
        panic!("one set_cursor for the first frame");
    };
    assert!(width > 0 && height > 0);
    f.frame(xid, 0x0000_FF00);
    f.retire();
    assert!(ui.take().is_empty(), "an unchanged cursor is not re-sent");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_define_cursor_sends_the_new_cursor_without_other_damage() {
    let Some((mut f, ui, _)) = cursor_or_skip() else {
        return;
    };
    let [(first, _, _)] = shown(&ui.take())[..] else {
        panic!("the root cursor");
    };
    let cursor = f.cursor((3, 4));
    let root = f.root();
    f.b.define_cursor(None, root, cursor)
        .expect("define_cursor");
    assert!(f.compose_due(), "a cursor change alone asks for a frame");
    f.tick();
    let [(version, hot, size)] = shown(&ui.take())[..] else {
        panic!("one set_cursor for the new cursor");
    };
    assert_ne!(version, first);
    assert_eq!((hot, size), ((3, 4), (16, 16)));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_hide_cursor_hides_and_show_cursor_restores() {
    let Some((mut f, ui, _)) = cursor_or_skip() else {
        return;
    };
    let [(first, _, _)] = shown(&ui.take())[..] else {
        panic!("the root cursor");
    };
    f.b.set_cursor_hidden(true);
    assert!(f.compose_due(), "hiding asks for a frame");
    f.tick();
    assert_eq!(ui.take(), [UiCall::Cursor(None)]);
    f.b.set_cursor_hidden(false);
    assert!(f.compose_due(), "showing asks for a frame");
    f.tick();
    let [(version, _, _)] = shown(&ui.take())[..] else {
        panic!("shown again");
    };
    assert_eq!(version, first, "the same cursor");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_pointer_motion_composes_nothing_and_sends_no_cursor() {
    let Some((mut f, ui, _)) = cursor_or_skip() else {
        return;
    };
    ui.take();
    for event in announcements(0) {
        handle_host_input(&mut f.state, &mut f.b, event);
    }
    let presented = f.output().frames_presented();
    for step in 0..100 {
        let raw = RawPointer {
            x_pt: 30.0 + f64::from(step % 10),
            y_pt: 10.0,
            view_h_pt: 48.0,
            scale: 1.0,
            dx_pt: 1.0,
            dy_pt: 0.0,
        };
        handle_host_input(&mut f.state, &mut f.b, motion(&raw, (64, 48), step));
    }
    assert!(
        !f.compose_due(),
        "motion alone does not wake the compositor"
    );
    f.b.maybe_composite().expect("maybe_composite");
    assert_eq!(f.output().frames_presented(), presented, "no frame");
    assert!(ui.take().is_empty(), "no cursor update");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_warp_pointer_moves_the_host_pointer() {
    let Some((mut f, ui, _)) = cursor_or_skip() else {
        return;
    };
    ui.take();
    f.b.warp_pointer_root(&mut f.state, 10, 20);
    assert_eq!(ui.take(), [UiCall::Warp(10, 20)]);
    f.b.warp_pointer_root(&mut f.state, 500, -3);
    assert_eq!(ui.take(), [UiCall::Warp(63, 0)], "clamped to the screen");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_animated_cursor_steps_versions() {
    let Some((mut f, ui, _)) = cursor_or_skip() else {
        return;
    };
    ui.take();
    let frames = [
        (CursorHandle::from_raw(f.cursor((1, 1))).expect("frame"), 1),
        (CursorHandle::from_raw(f.cursor((2, 2))).expect("frame"), 1),
    ];
    let anim =
        f.b.create_anim_cursor(None, &frames)
            .expect("create_anim_cursor")
            .expect("an animated cursor")
            .as_raw();
    let root = f.root();
    f.b.define_cursor(None, root, anim).expect("define_cursor");
    f.tick();
    let mut versions = shown(&ui.take())
        .into_iter()
        .map(|(version, _, _)| version)
        .collect::<Vec<_>>();
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(5));
        assert!(f.compose_due(), "the next frame is due");
        f.tick();
        versions.extend(shown(&ui.take()).into_iter().map(|(v, _, _)| v));
    }
    assert_eq!(
        versions.len(),
        3,
        "one cursor per animation frame: {versions:?}"
    );
    assert!(
        versions.windows(2).all(|pair| pair[0] < pair[1]),
        "versions step: {versions:?}"
    );
}
