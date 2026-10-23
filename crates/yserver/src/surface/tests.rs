//! The surface platform: sink behaviour without a GPU, the headless backend
//! drawing on lavapipe, and a `VK_EXT_headless_surface` swapchain driven
//! through real compose → copy → present → retire cycles.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use yserver_core::{
    backend::{Backend, BackendTopology, CursorHandle, RealizeBackground, WindowHandle},
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
    vk::{
        copy_scratch::CopyScratch,
        device::{VkContext, VkContextOptions},
    },
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
    b.realize_window_storage(None, xid, RealizeBackground::Paint)
        .expect("realize");
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
fn surface_publishes_the_client_count_to_the_shell() {
    let Some(mut b) = headless_or_skip() else {
        return;
    };
    let shell = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    b.pstate.client_count = Arc::clone(&shell);
    b.note_client_count(2);
    assert_eq!(shell.load(std::sync::atomic::Ordering::Acquire), 2);
    b.note_client_count(0);
    assert_eq!(shell.load(std::sync::atomic::Ordering::Acquire), 0);
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

/// A child of `parent` at `rect` with background `pixel`, mapped and realized.
fn create_child(
    b: &mut XBackend<SurfacePlatform>,
    parent: u32,
    rect: (i16, i16, u16, u16),
    pixel: u32,
    background: RealizeBackground,
) -> u32 {
    let window = b
        .create_subwindow(
            None,
            WindowHandle::from_raw(parent).expect("parent"),
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(pixel),
            None,
        )
        .expect("create_subwindow");
    let xid = window.as_raw();
    b.map_subwindow(None, xid).expect("map");
    b.realize_window_storage(None, xid, background)
        .expect("realize");
    xid
}

/// Xorg's `inhibitBGPaint` (`mi/miexpose.c:435`): a window realized without
/// its background paint shows its parent's pixels, not its background.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn realize_without_background_paint_shows_the_parent() {
    let Some(mut b) = headless_or_skip() else {
        return;
    };
    let top = create_window(&mut b);
    fill(&mut b, top, 0x00FF_0000, (0, 0, 8, 8));
    let painted = create_child(
        &mut b,
        top,
        (0, 0, 4, 4),
        0x0080_8080,
        RealizeBackground::Paint,
    );
    let inhibited = create_child(
        &mut b,
        top,
        (4, 4, 4, 4),
        0x0080_8080,
        RealizeBackground::Inhibited,
    );
    b.maybe_composite().expect("compose over no outputs");

    let first_pixel = |b: &mut XBackend<SurfacePlatform>, xid: u32| {
        let reply = b
            .get_image(None, xid, 2, 0, 0, 4, 4, !0)
            .expect("get_image")
            .expect("reply");
        reply[32..35].to_vec()
    };
    assert_eq!(first_pixel(&mut b, painted), [0x80, 0x80, 0x80]);
    assert_eq!(first_pixel(&mut b, inhibited), [0x00, 0x00, 0xFF]);
}

/// Xorg's `compRestoreWindow` (`composite/compalloc.c:232-256`): what was
/// drawn into the backing is the window's content once it is unredirected.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn unredirect_restores_what_was_drawn_into_the_backing() {
    let Some(mut b) = headless_or_skip() else {
        return;
    };
    let xid = create_window(&mut b);
    let backing = b
        .allocate_redirected_backing(None, WindowHandle::from_raw(xid).expect("window"), 8, 8, 24)
        .expect("allocate_redirected_backing");
    fill(&mut b, xid, 0x0000_FF00, (0, 0, 8, 8));
    b.release_redirected_backing(None, backing)
        .expect("release_redirected_backing");
    b.maybe_composite().expect("compose over no outputs");

    let reply = b
        .get_image(None, xid, 2, 0, 0, 8, 8, !0)
        .expect("get_image")
        .expect("reply");
    let pixels = &reply[32..];
    assert!(
        pixels.chunks_exact(4).all(|p| p[..3] == [0x00, 0xFF, 0x00]),
        "the whole window holds the backing's green"
    );
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
fn a_swapchain_build_commits_the_native_surface() {
    let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&commits);
    let native = NativeSurface::headless().with_commit(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    let vk = match native.create_context() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no presenting Vk: {e}");
            return;
        }
    };
    let link = Arc::new(ShellLink::default());
    let clock = Arc::new(FrameClock::new(link));
    let extent = vk::Extent2D {
        width: 64,
        height: 48,
    };
    let _output = SurfaceOutput::new(vk, &native, extent, clock).expect("headless surface output");
    assert_eq!(commits.load(std::sync::atomic::Ordering::Relaxed), 1);
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

/// A 2x copy shows every slot pixel as 2x2 image pixels and clears what
/// the scaled slot does not cover.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_scaled_copy_doubles_every_slot_pixel() {
    let Some(mut f) = surface_or_skip() else {
        return;
    };
    let xid = create_window(&mut f.b);
    f.frame(xid, 0x00FF_0000);
    f.retire();
    let slot = f.read_slot(rect(0, 0, 64, 48));
    // 256x256, larger than the 128x96 the slot covers.
    let scratch = CopyScratch::new(Arc::clone(f.output().vk())).expect("scratch image");
    f.output()
        .copy_on_screen_into(scratch.image(), scratch.extent(), 2)
        .expect("scaled copy");
    let (width, height) = (130, 98);
    let scaled =
        f.b.read_image_region(scratch.image(), rect(0, 0, width, height))
            .expect("image read");
    let px = |image: &[u8], stride: u32, x: u32, y: u32| {
        let at = ((y * stride + x) * 4) as usize;
        image[at..at + 4].to_vec()
    };
    assert_ne!(
        px(&slot, 64, 0, 0),
        px(&slot, 64, 5, 5),
        "a window on the root"
    );
    for y in 0..height {
        for x in 0..width {
            let got = px(&scaled, width, x, y);
            if x < 128 && y < 96 {
                assert_eq!(got, px(&slot, 64, x / 2, y / 2), "({x}, {y})");
            } else {
                assert_eq!(got[..3], [0, 0, 0], "({x}, {y}) cleared");
            }
        }
    }
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

/// Rootless: no output, the platform clock drives Present's MSC.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_clock_drives_msc_without_outputs() {
    let Some(mut b) = headless_or_skip() else {
        return;
    };
    let link = Arc::new(ShellLink::default());
    let (_poll, sender, rx) = yserver_core::core_loop::channel().expect("channel");
    link.connect(sender);
    let clock = Arc::new(FrameClock::new(Arc::clone(&link)));
    b.platform.set_clock(Arc::clone(&clock));
    let wakes = |rx: &yserver_core::core_loop::CoreReceiver| {
        rx.try_recv_all()
            .filter(|m| matches!(m, yserver_core::core_loop::Message::PlatformEvent))
            .count()
    };
    for ust in [16_000, 32_000, 48_000] {
        clock.tick(ust);
    }
    assert_eq!(b.present_get_ust_msc(2), (3, 48_000));
    assert_eq!(b.arm_idle_vblanks(2, &[5]).expect("arm"), 1);
    clock.tick(64_000);
    clock.tick(80_000);
    assert_eq!(wakes(&rx), 1, "msc 5 wakes the core once");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn headless_without_a_clock_is_unchanged() {
    let Some(mut b) = headless_or_skip() else {
        return;
    };
    assert_eq!(b.present_get_ust_msc(2), (0, 0));
    assert_eq!(b.arm_idle_vblanks(2, &[5]).expect("arm"), 0);
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
    assert_eq!(output.mode_ids.len(), modes.len());
    assert_eq!(
        output.mode_ids[0], output.mode_id,
        "the current size is preferred"
    );
    let mode = &modes[0];
    assert_eq!((mode.width, mode.height, mode.vrefresh), (64, 48, 120));
    assert_eq!(
        modes.len(),
        9,
        "and every fixed mode fits a headless output"
    );
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
            internal: false,
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
    ContentSize(u16, u16, u64),
}

#[derive(Default)]
struct RecordingUi(std::sync::Mutex<Vec<UiCall>>);

impl RecordingUi {
    fn take(&self) -> Vec<UiCall> {
        std::mem::take(&mut *self.0.lock().expect("ui calls"))
    }
}

impl ShellUi for RecordingUi {
    fn set_display(&self, _display: u16) {}

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

    fn set_content_size(&self, width: u16, height: u16, token: u64) {
        self.0
            .lock()
            .expect("ui calls")
            .push(UiCall::ContentSize(width, height, token));
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

/// A host that takes no frames: the rootless cursor tests need none.
struct NoFramesHost;

impl crate::rootless::host::RootlessHost for NoFramesHost {
    fn handoff(
        &self,
        _id: yserver_core::rootless::ToplevelId,
        _depth: u8,
    ) -> crate::rootless::host::HandoffKind {
        crate::rootless::host::HandoffKind::Cpu
    }

    fn create_surface(
        &mut self,
        _id: yserver_core::rootless::ToplevelId,
        _vk: &VkContext,
    ) -> std::io::Result<vk::SurfaceKHR> {
        Err(std::io::Error::other("no surfaces"))
    }

    fn submit_cpu(&mut self, frame: crate::rootless::host::CpuFrame) {
        drop(frame);
    }

    fn first_frame(&mut self, _id: yserver_core::rootless::ToplevelId) {}

    fn on_events(&mut self, _events: &[yserver_core::rootless::RootlessEvent]) {}
}

/// A rootless server (no outputs) with a recording UI.
fn rootless_or_skip() -> Option<(XBackend<SurfacePlatform>, ServerState, Arc<RecordingUi>)> {
    let mut b = headless_or_skip()?;
    let ui = Arc::new(RecordingUi::default());
    b.pstate.ui = Some(Arc::clone(&ui) as Arc<dyn ShellUi>);
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    crate::rootless::start(
        &mut state,
        &mut b,
        Box::new(NoFramesHost),
        yserver_core::rootless::CoordinatePolicy::Global { origin: (0, 0) },
    )
    .expect("rootless start");
    Some((b, state, ui))
}

/// A 16x16 bitmap cursor with hotspot `hot`.
fn bitmap_cursor(b: &mut XBackend<SurfacePlatform>, hot: (u16, u16)) -> u32 {
    let pixmap = b.create_pixmap(None, 1, 16, 16).expect("pixmap");
    b.create_cursor(None, pixmap, None, (0xFFFF, 0, 0), (0, 0, 0), hot.0, hot.1)
        .expect("create_cursor")
        .as_raw()
}

/// Rootless has no cursor plane: the shell still gets the sprite.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_shell_gets_the_root_cursor() {
    let Some((mut b, _state, ui)) = rootless_or_skip() else {
        return;
    };
    b.maybe_composite().expect("maybe_composite");
    let root = b.get_active_cursor_image().expect("a root cursor");
    assert_eq!(
        shown(&ui.take()),
        [(
            u64::from(root.serial),
            (root.hot_x, root.hot_y),
            (u32::from(root.width), u32::from(root.height))
        )],
        "the root cursor, once"
    );
    b.maybe_composite().expect("maybe_composite");
    assert!(ui.take().is_empty(), "an unchanged cursor is not re-sent");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_define_and_hide_cursor() {
    let Some((mut b, _state, ui)) = rootless_or_skip() else {
        return;
    };
    b.maybe_composite().expect("maybe_composite");
    let [(first, _, _)] = shown(&ui.take())[..] else {
        panic!("the root cursor");
    };
    let xid = create_window(&mut b);
    b.core.prev_pointer_window = Some(xid);
    let cursor = bitmap_cursor(&mut b, (3, 4));
    b.define_cursor(None, xid, cursor).expect("define_cursor");
    b.maybe_composite().expect("maybe_composite");
    let [(version, hot, size)] = shown(&ui.take())[..] else {
        panic!("one set_cursor for the window's cursor");
    };
    assert_ne!(version, first);
    assert_eq!((hot, size), ((3, 4), (16, 16)));
    b.set_cursor_hidden(true);
    b.maybe_composite().expect("maybe_composite");
    assert_eq!(ui.take(), [UiCall::Cursor(None)]);
    b.set_cursor_hidden(false);
    b.maybe_composite().expect("maybe_composite");
    assert_eq!(
        shown(&ui.take()),
        [(version, (3, 4), (16, 16))],
        "shown again"
    );
    b.maybe_composite().expect("maybe_composite");
    b.maybe_composite().expect("maybe_composite");
    assert!(ui.take().is_empty(), "idle tails send nothing");
}

/// Rootless has no output: an animated cursor still wakes the loop at its
/// next frame and advances then.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_animated_cursor_wakes_and_advances() {
    let Some((mut b, _state, ui)) = rootless_or_skip() else {
        return;
    };
    b.maybe_composite().expect("maybe_composite");
    ui.take();
    let frames = [
        (
            CursorHandle::from_raw(bitmap_cursor(&mut b, (1, 1))).expect("frame"),
            1,
        ),
        (
            CursorHandle::from_raw(bitmap_cursor(&mut b, (2, 2))).expect("frame"),
            1,
        ),
    ];
    let anim = b
        .create_anim_cursor(None, &frames)
        .expect("create_anim_cursor")
        .expect("an animated cursor")
        .as_raw();
    let root = b.core.window_id;
    b.define_cursor(None, root, anim).expect("define_cursor");
    b.maybe_composite().expect("maybe_composite");
    let mut versions: Vec<u64> = shown(&ui.take()).into_iter().map(|(v, _, _)| v).collect();
    // Serve only the loop's own wakeups: with nothing else going on, the
    // animation must keep asking for them (1 ms frames).
    let until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < until {
        let deadline = SurfacePlatform::next_wakeup(&b).expect("a wakeup for the next frame");
        if let Some(wait) = deadline.min(until).checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        b.maybe_composite().expect("maybe_composite");
        versions.extend(shown(&ui.take()).into_iter().map(|(v, _, _)| v));
    }
    assert!(versions.len() >= 3, "the frames advance: {versions:?}");
    assert!(versions.windows(2).all(|p| p[0] < p[1]), "{versions:?}");
}

/// XTEST motion moves the host pointer, as on Xorg; real device motion
/// does not (it came from there).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_xtest_motion_warps_the_host_pointer() {
    let Some((mut b, mut state, ui)) = rootless_or_skip() else {
        return;
    };
    for event in announcements(0) {
        handle_host_input(&mut state, &mut b, event);
    }
    ui.take();
    let raw = RawPointer {
        x_pt: 30.0,
        y_pt: 10.0,
        view_h_pt: 48.0,
        scale: 1.0,
        dx_pt: 1.0,
        dy_pt: 0.0,
    };
    handle_host_input(&mut state, &mut b, motion(&raw, (64, 48), 1));
    assert!(ui.take().is_empty(), "device motion warps nothing");
    // What XTEST FakeInput MotionNotify does (process_request/xtest.rs).
    b.on_host_input(
        &mut state,
        HostInputEvent::PointerMotion {
            origin: yserver_core::core_loop::InputOrigin::XTest(
                yserver_core::xinput::DEVICEID_XTEST_POINTER,
            ),
            x: 10,
            y: 20,
            time: 2,
            relative: false,
            dx: 0,
            dy: 0,
            motion_delta: None,
        },
    );
    b.resync_input_position();
    assert_eq!(ui.take(), [UiCall::Warp(10, 20)]);
    b.warp_pointer_root(&mut state, 5, 6);
    assert_eq!(ui.take(), [UiCall::Warp(5, 6)], "WarpPointer, as before");
}

/// A server over a backend, and one client (id 9) that selected
/// StructureNotify on root and RRScreenChangeNotify, talking through the
/// core's request dispatch.
struct ScreenFixture {
    b: XBackend<SurfacePlatform>,
    state: ServerState,
    peer: std::os::unix::net::UnixStream,
    sequence: u16,
    /// The events that came with the last `set_crtc_config` reply.
    pending: Vec<[u8; 32]>,
}

/// What the client read: replies (the bytes after the 32-byte header
/// included), errors as `(code, major)` and events.
#[derive(Debug, Default)]
struct Received {
    replies: Vec<Vec<u8>>,
    errors: Vec<(u8, u8)>,
    /// The error value of each of `errors`.
    error_values: Vec<u32>,
    events: Vec<[u8; 32]>,
}

type Sizes = Vec<(u16, u16)>;

const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
const CONFIGURE_NOTIFY: u8 = 22;
const RR_SCREEN_CHANGE_NOTIFY: u8 = 89;
const ROOT_BACKGROUND: [u8; 3] = [0x50, 0x50, 0x50];
const GC: u32 = 0x0020_0001;

fn screen_fixture(b: XBackend<SurfacePlatform>) -> ScreenFixture {
    let mut b = b;
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    let peer = add_client(&mut state, 9, STRUCTURE_NOTIFY);
    let mut f = ScreenFixture {
        b,
        state,
        peer,
        sequence: 0,
        pending: Vec::new(),
    };
    // RRSelectInput(root, ScreenChangeNotifyMask | CrtcChangeNotifyMask)
    f.randr(4, &[root_le(), [3, 0, 0, 0]].concat());
    // CreateGC(GC, root, no values); `fill` sets the foreground.
    f.request(55, 0, &[GC.to_le_bytes(), root_le(), [0; 4]].concat());
    assert!(f.received().errors.is_empty(), "setup raised an error");
    f
}

fn root_le() -> [u8; 4] {
    yserver_core::resources::ROOT_WINDOW.0.to_le_bytes()
}

impl ScreenFixture {
    fn request(&mut self, opcode: u8, data: u8, body: &[u8]) {
        use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};
        assert_eq!(body.len() % 4, 0, "padded body");
        self.sequence += 1;
        yserver_core::core_loop::process_request::process_request(
            &mut self.state,
            &mut self.b as &mut dyn Backend,
            ClientId(9),
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
    }

    fn randr(&mut self, minor: u8, body: &[u8]) {
        self.request(128, minor, body);
    }

    /// RRSetScreenSize at 96 DPI-ish millimetres.
    fn set_screen_size(&mut self, width: u16, height: u16) {
        let body = [
            root_le().as_slice(),
            &width.to_le_bytes(),
            &height.to_le_bytes(),
            &u32::from(width / 4).to_le_bytes(),
            &u32::from(height / 4).to_le_bytes(),
        ]
        .concat();
        self.randr(7, &body);
    }

    /// GetImage (ZPixmap, all planes) of `drawable`.
    fn get_image(&mut self, drawable: u32, rect: (i16, i16, u16, u16)) {
        let body = [
            drawable.to_le_bytes().as_slice(),
            &rect.0.to_le_bytes(),
            &rect.1.to_le_bytes(),
            &rect.2.to_le_bytes(),
            &rect.3.to_le_bytes(),
            &u32::MAX.to_le_bytes(),
        ]
        .concat();
        self.request(73, 2, &body);
    }

    /// `rect` of the root as ZPixmap rows of BGRX, or the error code.
    fn root_image(&mut self, rect: (i16, i16, u16, u16)) -> Result<Vec<u8>, u8> {
        self.get_image(yserver_core::resources::ROOT_WINDOW.0, rect);
        let mut got = self.received();
        match (got.replies.pop(), got.errors.pop()) {
            (Some(reply), None) => Ok(reply[32..].to_vec()),
            (None, Some((code, _))) => Err(code),
            other => panic!("one reply or error: {other:?}"),
        }
    }

    /// PolyFillRectangle `rect` of `drawable` with `pixel`.
    fn fill(&mut self, drawable: u32, pixel: u32, rect: (i16, i16, u16, u16)) {
        // ChangeGC(GC, GCForeground)
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

    fn received(&mut self) -> Received {
        use std::io::Read;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 65536];
        while let Ok(n) = self.peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        let mut got = Received::default();
        let mut rest = bytes.as_slice();
        while rest.len() >= 32 {
            let len = match rest[0] {
                1 => 32 + 4 * u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize,
                _ => 32,
            };
            let (message, tail) = rest.split_at(len);
            match message[0] {
                0 => {
                    got.errors.push((message[1], message[10]));
                    got.error_values
                        .push(u32::from_le_bytes(message[4..8].try_into().unwrap()));
                }
                1 => got.replies.push(message.to_vec()),
                _ => got.events.push(message.try_into().unwrap()),
            }
            rest = tail;
        }
        assert!(rest.is_empty(), "a whole number of messages");
        got
    }

    /// The `(width, height)` of the root ConfigureNotify and of the
    /// RRScreenChangeNotify among `events`.
    fn resize_events(events: &[[u8; 32]]) -> (Sizes, Sizes) {
        let size = |ev: &[u8; 32], at: usize| {
            (
                u16::from_le_bytes([ev[at], ev[at + 1]]),
                u16::from_le_bytes([ev[at + 2], ev[at + 3]]),
            )
        };
        let configure = events
            .iter()
            .filter(|ev| {
                ev[0] & 0x7f == CONFIGURE_NOTIFY
                    && ev[8..12] == yserver_core::resources::ROOT_WINDOW.0.to_le_bytes()
            })
            .map(|ev| size(ev, 20))
            .collect();
        let screen = events
            .iter()
            .filter(|ev| ev[0] & 0x7f == RR_SCREEN_CHANGE_NOTIFY)
            .map(|ev| size(ev, 24))
            .collect();
        (configure, screen)
    }

    /// RRSetScreenSize, asserting it raised no error and that the client
    /// heard of the new size once each way.
    fn resize(&mut self, width: u16, height: u16) {
        self.received();
        self.set_screen_size(width, height);
        let got = self.received();
        assert!(got.errors.is_empty(), "RRSetScreenSize: {:?}", got.errors);
        let (configure, screen) = Self::resize_events(&got.events);
        assert_eq!(configure, [(width, height)], "root ConfigureNotify");
        assert_eq!(screen, [(width, height)], "RRScreenChangeNotify");
        let root = self
            .state
            .resources
            .window(yserver_core::resources::ROOT_WINDOW)
            .expect("root");
        assert_eq!((root.width, root.height), (width, height));
    }
}

/// Every pixel of a BGRX row block has the RGB `bgr`.
fn all_pixels(image: &[u8], bgr: [u8; 3]) -> bool {
    !image.is_empty() && image.chunks_exact(4).all(|px| px[..3] == bgr)
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rr_set_screen_size_grows_the_root_with_its_background() {
    let Some(b) = headless_or_skip() else {
        return;
    };
    let mut f = screen_fixture(b);
    assert!(
        all_pixels(
            &f.root_image((0, 0, 16, 16)).expect("root"),
            ROOT_BACKGROUND
        ),
        "the headless root is the default background"
    );
    f.resize(400, 300);
    for rect in [(380, 280, 20, 20), (64, 0, 16, 16), (0, 48, 16, 16)] {
        let image = f.root_image(rect).expect("GetImage of the grown area");
        assert!(
            all_pixels(&image, ROOT_BACKGROUND),
            "the grown area {rect:?} is the root background"
        );
    }
    assert_eq!(f.b.fb_dimensions(), (400, 300));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rr_set_screen_size_grown_root_area_takes_drawing() {
    let Some(b) = headless_or_skip() else {
        return;
    };
    let mut f = screen_fixture(b);
    f.resize(400, 300);
    f.fill(
        yserver_core::resources::ROOT_WINDOW.0,
        0x00FF_0000,
        (300, 200, 20, 20),
    );
    let image = f.root_image((300, 200, 20, 20)).expect("GetImage");
    assert!(all_pixels(&image, [0x00, 0x00, 0xFF]), "red where drawn");
    let image = f.root_image((320, 220, 16, 16)).expect("GetImage");
    assert!(all_pixels(&image, ROOT_BACKGROUND), "background next to it");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rr_set_screen_size_shrinks_and_grows_again_keeping_clients() {
    let Some(b) = headless_or_skip() else {
        return;
    };
    let mut f = screen_fixture(b);
    f.resize(400, 300);
    // CreateWindow(0x0020_0002, root, 360, 240, 32x32, InputOutput,
    // CWBackPixel green) + MapWindow
    let window = 0x0020_0002u32;
    let body = [
        window.to_le_bytes().as_slice(),
        &root_le(),
        &360i16.to_le_bytes(),
        &240i16.to_le_bytes(),
        &32u16.to_le_bytes(),
        &32u16.to_le_bytes(),
        &0u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &0u32.to_le_bytes(),
        &2u32.to_le_bytes(),
        &0x0000_FF00u32.to_le_bytes(),
    ]
    .concat();
    f.request(1, 0, &body);
    f.request(8, 0, &window.to_le_bytes());
    assert!(f.received().errors.is_empty(), "the window was created");

    f.resize(320, 200);
    let image = f.root_image((300, 180, 20, 20)).expect("GetImage");
    assert!(all_pixels(&image, ROOT_BACKGROUND));
    f.resize(480, 360);
    let image = f
        .root_image((440, 320, 40, 40))
        .expect("GetImage of the regrown area");
    assert!(all_pixels(&image, ROOT_BACKGROUND));

    assert_eq!(f.b.fb_dimensions(), (480, 360));

    // GetGeometry: the window outside the shrunk screen is still there.
    f.received();
    f.request(14, 0, &window.to_le_bytes());
    let got = f.received();
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    let reply = &got.replies[0];
    let geometry = (
        i16::from_le_bytes([reply[12], reply[13]]),
        i16::from_le_bytes([reply[14], reply[15]]),
        u16::from_le_bytes([reply[16], reply[17]]),
        u16::from_le_bytes([reply[18], reply[19]]),
    );
    assert_eq!(geometry, (360, 240, 32, 32));
    // GetWindowAttributes: still mapped and viewable.
    f.request(3, 0, &window.to_le_bytes());
    assert_eq!(f.received().replies[0][26], 2, "map state Viewable");
    f.get_image(window, (0, 0, 32, 32));
    let got = f.received();
    assert!(
        got.errors.is_empty(),
        "GetImage of the window: {:?}",
        got.errors
    );
}

const CRTC_CHANGE_NOTIFY: (u8, u8) = (90, 0);
const BAD_MATCH: u8 = 8;
const BAD_VALUE: u8 = 2;
/// `RRSetConfigFailed`, the status of a CRTC set the DDX refused.
const SET_CONFIG_FAILED: u8 = 3;

/// A presenting window (64x48) with a recording UI, its link and clock,
/// and the client of [`screen_fixture`].
struct WindowFixture {
    f: ScreenFixture,
    link: Arc<ShellLink>,
    clock: Arc<FrameClock>,
    ui: Arc<RecordingUi>,
}

fn window_or_skip() -> Option<WindowFixture> {
    let SurfaceFixture {
        mut b, clock, link, ..
    } = surface_or_skip()?;
    let ui = Arc::new(RecordingUi::default());
    b.pstate.ui = Some(Arc::clone(&ui) as Arc<dyn ShellUi>);
    Some(WindowFixture {
        f: screen_fixture(b),
        link,
        clock,
        ui,
    })
}

impl ScreenFixture {
    /// RRSetCrtcConfig of the window's CRTC: the reply status, or the
    /// error `(code, value)`.
    fn set_crtc_config(
        &mut self,
        size: (u16, u16),
        x: i16,
        rotation: u16,
    ) -> Result<u8, (u8, u32)> {
        let output = self.state.randr.outputs[0].clone();
        let mode = self
            .state
            .randr
            .mode_table
            .iter()
            .find(|m| (m.width, m.height) == size)
            .unwrap_or_else(|| panic!("a {size:?} mode"))
            .mode_id;
        let body = [
            output.crtc_id.to_le_bytes().as_slice(),
            &0u32.to_le_bytes(),
            &0u32.to_le_bytes(),
            &x.to_le_bytes(),
            &0i16.to_le_bytes(),
            &mode.to_le_bytes(),
            &rotation.to_le_bytes(),
            &[0, 0],
            &output.output_id.to_le_bytes(),
        ]
        .concat();
        self.received();
        self.randr(21, &body);
        let mut got = self.received();
        if let (Some(error), Some(value)) = (got.errors.pop(), got.error_values.pop()) {
            return Err((error.0, value));
        }
        let reply = got.replies.pop().expect("a SetCrtcConfig reply");
        self.pending = got.events;
        Ok(reply[1])
    }

    /// The error code of an RRSetScreenSize, if any.
    fn screen_size_error(&mut self, width: u16, height: u16) -> Option<u8> {
        self.received();
        self.set_screen_size(width, height);
        self.received().errors.pop().map(|(code, _)| code)
    }

    /// `(width, height)` of the CRTC mode, the slots, the scene layout's
    /// root rect and the RandR screen.
    fn sizes(&self) -> [(u32, u32); 4] {
        let output = &self.state.randr.outputs[0];
        let (slots, _) = self.b.platform.outputs[0].extents();
        let layouts = self.b.platform.output_layouts();
        let (_, _, lw, lh) = layouts[0].root_rect;
        [
            (u32::from(output.width), u32::from(output.height)),
            (slots.width, slots.height),
            (lw, lh),
            (
                u32::from(self.state.randr.screen_width),
                u32::from(self.state.randr.screen_height),
            ),
        ]
    }

    fn crtc_notifies(events: &[[u8; 32]]) -> Vec<(u16, u16)> {
        events
            .iter()
            .filter(|ev| (ev[0] & 0x7f, ev[1]) == CRTC_CHANGE_NOTIFY)
            .map(|ev| {
                (
                    u16::from_le_bytes([ev[28], ev[29]]),
                    u16::from_le_bytes([ev[30], ev[31]]),
                )
            })
            .collect()
    }
}

/// Wide `(w, h)` pairs as `u32`, for [`ScreenFixture::sizes`].
fn wide(size: (u16, u16)) -> (u32, u32) {
    (u32::from(size.0), u32::from(size.1))
}

impl WindowFixture {
    /// Compose and present until the frame retired.
    fn frame(&mut self) {
        let f = &mut self.f;
        f.b.maybe_composite().expect("maybe_composite");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ust = 0;
        while f.b.platform.outputs[0].has_pending() {
            assert!(Instant::now() < deadline, "the frame never retired");
            std::thread::sleep(Duration::from_millis(1));
            ust += 1000;
            if self.clock.tick(ust) {
                f.b.on_platform_event(&mut f.state);
            }
        }
    }

    /// The shell reports the window at `size`, scale 1, millimetres
    /// unknown; what the client heard.
    fn host_resize(&mut self, size: (u16, u16), request: Option<u64>) -> Received {
        self.host_event(size, 1, (0, 0), request)
    }

    /// The shell reports the window at `size`, `scale` and `mm`; what the
    /// client heard.
    fn host_event(
        &mut self,
        size: (u16, u16),
        scale: u32,
        mm: (u32, u32),
        request: Option<u64>,
    ) -> Received {
        self.f.received();
        self.link.post(ShellEvent::Resized {
            width: size.0,
            height: size.1,
            scale,
            mm,
            request,
        });
        self.f.b.on_platform_event(&mut self.f.state);
        self.f.received()
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rr_set_screen_size_resizes_the_root_only() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    let output = (64, 48);
    for root in [(400u16, 300u16), (320, 200)] {
        w.f.resize(root.0, root.1);
        assert_eq!(
            w.f.sizes(),
            [wide(output), wide(output), wide(output), wide(root)],
            "CRTC mode, slots and layout keep the output size under a {root:?} root"
        );
        w.frame();
        assert_eq!(
            w.f.b.platform.outputs[0].extents().1,
            Some(vk::Extent2D {
                width: 64,
                height: 48
            }),
            "the swapchain keeps the output size"
        );
        let corner = rect(44, 28, 20, 20);
        let shown = SurfacePlatform::read_on_screen_region(&mut w.f.b, corner).expect("read");
        assert!(
            all_pixels(&shown, ROOT_BACKGROUND),
            "the output's far corner shows the root"
        );
        let image =
            w.f.root_image((i16::try_from(root.0).unwrap() - 16, 0, 16, 16))
                .expect("GetImage of the root beyond the output");
        assert!(all_pixels(&image, ROOT_BACKGROUND));
    }
    assert!(
        w.ui.take()
            .iter()
            .all(|call| !matches!(call, UiCall::ContentSize(..)))
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rr_set_crtc_config_switches_the_window_mode() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    w.f.resize(1280, 800);
    let modes: Vec<(u16, u16)> = w.f.state.randr.outputs[0]
        .mode_ids
        .iter()
        .map(|id| {
            let mode =
                w.f.state
                    .randr
                    .mode_table
                    .iter()
                    .find(|m| m.mode_id == *id)
                    .unwrap();
            (mode.width, mode.height)
        })
        .collect();
    assert_eq!(modes[0], (64, 48), "the current size is the preferred mode");
    assert!(modes.contains(&(1024, 768)) && modes.contains(&(2560, 1600)));

    assert_eq!(w.f.set_crtc_config((1024, 768), 0, 1), Ok(0));
    let events = std::mem::take(&mut w.f.pending);
    assert_eq!(ScreenFixture::crtc_notifies(&events), [(1024, 768)]);
    assert_eq!(
        ScreenFixture::resize_events(&events).0,
        Vec::<(u16, u16)>::new(),
        "no root ConfigureNotify"
    );
    assert_eq!(
        w.f.sizes(),
        [(1024, 768), (1024, 768), (1024, 768), (1280, 800)]
    );
    assert_eq!(w.ui.take().last(), Some(&UiCall::ContentSize(1024, 768, 1)));
    w.frame();
    assert_eq!(
        w.f.b.platform.outputs[0].extents().1,
        Some(vk::Extent2D {
            width: 1024,
            height: 768
        })
    );
    // The preferred mode follows the output.
    let output = &w.f.state.randr.outputs[0];
    assert_eq!(output.mode_ids[0], output.mode_id);

    // Xorg's answers for a CRTC without rotations or transforms:
    // `(~crtc->rotations) & rotation` is BadMatch (rrcrtc.c:1403-1410); a
    // mode past the screen is BadValue carrying x (rrcrtc.c:1446-1451); a
    // position the DDX refuses is RRSetConfigFailed (rrcrtc.c:1462-1465).
    assert_eq!(w.f.set_crtc_config((1024, 768), 0, 2), Err((BAD_MATCH, 2)));
    assert_eq!(w.f.set_crtc_config((1440, 900), 0, 1), Err((BAD_VALUE, 0)));
    assert_eq!(
        w.f.set_crtc_config((1024, 768), 8, 1),
        Ok(SET_CONFIG_FAILED)
    );
    // RRSetScreenSize below the output crops it: BadMatch (rrscreen.c:266-281).
    assert_eq!(w.f.screen_size_error(800, 600), Some(BAD_MATCH));
    assert_eq!(
        w.f.sizes(),
        [(1024, 768), (1024, 768), (1024, 768), (1280, 800)],
        "nothing changed"
    );
    assert!(
        w.ui.take()
            .iter()
            .all(|call| !matches!(call, UiCall::ContentSize(..))),
        "no window request"
    );

    // RRGetScreenSizeRange: 320x200 up to the image limit, at most 8192.
    w.f.received();
    w.f.randr(6, &root_le());
    let reply = w.f.received().replies.pop().expect("GetScreenSizeRange");
    let at = |o: usize| u16::from_le_bytes([reply[o], reply[o + 1]]);
    assert_eq!((at(8), at(10)), (320, 200));
    assert!(at(12) <= 8192 && at(12) >= 4096 && at(12) == at(14));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn failed_client_resizes_keep_the_old_state() {
    use crate::kms::render::{ResizeFault, inject_resize_fault};
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    w.f.b.get_overlay_window(None).expect("materialise the COW");
    w.f.resize(1280, 800);
    let before = w.f.sizes();
    let root = w.f.b.store.lookup(yserver_core::resources::ROOT_WINDOW.0);
    // Root allocation, COW allocation, root fill, COW fill.
    for (stage, skip) in [
        (ResizeFault::Alloc, 0),
        (ResizeFault::Alloc, 1),
        (ResizeFault::Fill, 0),
        (ResizeFault::Fill, 1),
    ] {
        inject_resize_fault(stage, skip);
        assert_eq!(
            w.f.screen_size_error(1600, 1000),
            Some(BAD_MATCH),
            "RRSetScreenSize with {stage:?} #{skip} failing (rrscreen.c:289-293)"
        );
        assert_eq!(w.f.sizes(), before, "{stage:?} #{skip}");
        assert_eq!(w.f.b.fb_dimensions(), (1280, 800));
        assert_eq!(
            w.f.b.store.lookup(yserver_core::resources::ROOT_WINDOW.0),
            root
        );
    }
    // Slot allocation, scene outputs: the DDX refused the CRTC set.
    for stage in [ResizeFault::Alloc, ResizeFault::Outputs] {
        inject_resize_fault(stage, 0);
        assert_eq!(
            w.f.set_crtc_config((1024, 768), 0, 1),
            Ok(SET_CONFIG_FAILED),
            "RRSetCrtcConfig with {stage:?} failing (rrcrtc.c:1462-1465)"
        );
        assert!(ScreenFixture::crtc_notifies(&w.f.pending).is_empty());
        assert_eq!(w.f.sizes(), before, "{stage:?}");
    }
    assert!(
        w.ui.take()
            .iter()
            .all(|call| !matches!(call, UiCall::ContentSize(..)))
    );
    // Still whole: the root draws and reads back, and a frame presents.
    let image = w.f.root_image((1200, 700, 16, 16)).expect("GetImage");
    assert!(all_pixels(&image, ROOT_BACKGROUND));
    w.frame();
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_user_window_resize_moves_root_and_output() {
    use crate::kms::render::{ResizeFault, inject_resize_fault};
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    let got = w.host_resize((400, 300), None);
    let (configure, screen) = ScreenFixture::resize_events(&got.events);
    assert_eq!(configure, [(400, 300)], "root ConfigureNotify");
    assert_eq!(screen, [(400, 300)], "RRScreenChangeNotify");
    assert_eq!(ScreenFixture::crtc_notifies(&got.events), [(400, 300)]);
    assert_eq!(w.f.sizes(), [(400, 300); 4]);
    assert_eq!(w.f.b.fb_dimensions(), (400, 300));
    let image = w.f.root_image((380, 280, 20, 20)).expect("GetImage");
    assert!(all_pixels(&image, ROOT_BACKGROUND));
    w.frame();

    let got = w.host_resize((400, 300), None);
    assert!(got.events.is_empty(), "the same size again: nothing");

    for (stage, skip) in [
        (ResizeFault::Alloc, 0),
        (ResizeFault::Fill, 0),
        (ResizeFault::Alloc, 1),
        (ResizeFault::Outputs, 0),
    ] {
        inject_resize_fault(stage, skip);
        let got = w.host_resize((500, 350), None);
        assert!(got.events.is_empty(), "{stage:?} #{skip}: no events");
        assert_eq!(w.f.sizes(), [(400, 300); 4], "{stage:?} #{skip}: old sizes");
    }
    // The next event retries.
    let got = w.host_resize((500, 350), None);
    assert_eq!(ScreenFixture::resize_events(&got.events).0, [(500, 350)]);
    assert_eq!(w.f.sizes(), [(500, 350); 4]);
    w.frame();
}

/// The shell resizes the drawable before posting, so every resize event
/// rebuilds the swapchain, even one that leaves the output alone.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn every_window_resize_event_rebuilds_the_swapchain() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    let old = w.f.b.platform.outputs[0].swapchain_handle();
    let output = w.f.b.platform.output_size;
    let got = w.host_resize(output, None);
    assert!(got.events.is_empty(), "the output keeps its size");
    w.frame();
    assert_ne!(
        w.f.b.platform.outputs[0].swapchain_handle(),
        old,
        "a new swapchain"
    );
}

/// A scale change alone keeps the X sizes and rebuilds the swapchain at
/// the new scale (a headless surface takes the size the copy asks for).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_scale_change_keeps_the_screen_and_scales_the_swapchain() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    let got = w.host_event((64, 48), 2, (0, 0), None);
    assert!(got.events.is_empty(), "nothing changed for X");
    w.frame();
    let (slots, swapchain) = w.f.b.platform.outputs[0].extents();
    assert_eq!((slots.width, slots.height), (64, 48));
    let swapchain = swapchain.expect("a swapchain");
    assert_eq!((swapchain.width, swapchain.height), (128, 96));
    assert_eq!(w.f.sizes(), [(64, 48); 4]);

    w.host_event((64, 48), 1, (0, 0), None);
    w.frame();
    let swapchain = w.f.b.platform.outputs[0].extents().1.expect("a swapchain");
    assert_eq!((swapchain.width, swapchain.height), (64, 48));
}

/// The output reports the millimetres the shell measured; the screen keeps
/// 96 DPI, as Xorg without a configured DisplaySize.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_window_output_reports_the_shells_millimetres() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    let output_mm = |w: &WindowFixture| {
        let info = w.f.state.randr.output_info(1, 0).expect("WINDOW-0");
        (info.width_mm, info.height_mm)
    };
    // 64x48 px at 96 DPI: (64*254 + 480) / 960 = 17, (48*254 + 480) / 960 = 13.
    assert_eq!(output_mm(&w), (17, 13), "unknown: 96 DPI");

    let got = w.host_event((64, 48), 1, (20, 15), None);
    assert_eq!(output_mm(&w), (20, 15));
    assert_eq!(
        ScreenFixture::resize_events(&got.events).1,
        [(64, 48)],
        "ScreenChangeNotify, as Xorg's TellChanged"
    );
    let got = w.host_event((64, 48), 1, (20, 15), None);
    assert!(got.events.is_empty(), "unchanged");

    w.host_event((128, 96), 1, (40, 30), None);
    assert_eq!(output_mm(&w), (40, 30));
    assert_eq!(
        (w.f.state.randr.width_mm, w.f.state.randr.height_mm),
        (34, 25),
        "the screen: (128*254 + 480) / 960, (96*254 + 480) / 960"
    );

    w.host_event((128, 96), 1, (0, 0), None);
    assert_eq!(output_mm(&w), (34, 25), "unknown again: 96 DPI");
}

/// After `--fb 400x300` over the 64x48 output, a user resize to 400x300
/// moves the output only: the screen already has the size (codex round 2).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_user_resize_to_the_screen_size_moves_only_the_output() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.f.resize(400, 300);
    let got = w.host_resize((400, 300), None);
    let (configure, screen) = ScreenFixture::resize_events(&got.events);
    assert!(configure.is_empty(), "no root ConfigureNotify");
    assert_eq!(
        screen,
        [(400, 300)],
        "ScreenChangeNotify, as Xorg's TellChanged"
    );
    assert_eq!(ScreenFixture::crtc_notifies(&got.events), [(400, 300)]);
    assert_eq!(w.f.sizes(), [(400, 300); 4]);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_clamped_window_request_moves_only_the_output() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    w.f.resize(1280, 800);
    assert_eq!(w.f.set_crtc_config((1024, 768), 0, 1), Ok(0));
    assert_eq!(w.ui.take().last(), Some(&UiCall::ContentSize(1024, 768, 1)));

    // The window system clamped the window to 1000x700.
    let got = w.host_resize((1000, 700), Some(1));
    assert!(ScreenFixture::resize_events(&got.events).0.is_empty());
    assert_eq!(ScreenFixture::crtc_notifies(&got.events), [(1000, 700)]);
    assert_eq!(
        w.f.sizes(),
        [(1000, 700), (1000, 700), (1000, 700), (1280, 800)],
        "the output follows the window, the root stays"
    );
    assert_eq!(w.f.b.fb_dimensions(), (1280, 800));
    w.frame();

    // The request is done: its token again is stale.
    let got = w.host_resize((900, 600), Some(1));
    assert!(got.events.is_empty());
    assert_eq!(w.f.sizes()[0], (1000, 700));
    // A completion larger than the root stops at the root.
    assert_eq!(w.f.set_crtc_config((1280, 800), 0, 1), Ok(0));
    let got = w.host_resize((1400, 900), Some(2));
    assert!(got.events.is_empty(), "the output already fills the root");
    assert_eq!(w.f.sizes()[0], (1280, 800));
}

#[test]
fn window_modes_are_the_current_size_then_the_fixed_ones_that_fit() {
    let (outputs, modes) = super::present::window_randr((1024, 768), (0, 0), (1440, 900), 60);
    let sizes: Vec<(u32, u16, u16)> = modes
        .iter()
        .map(|m| (m.mode_id, m.width, m.height))
        .collect();
    assert_eq!(sizes, [(11, 1024, 768), (9, 1440, 900), (10, 1280, 800)]);
    let output = &outputs[0];
    assert_eq!((output.mode_id, output.num_preferred), (11, 1));
    assert_eq!(output.mode_ids, [11, 9, 10]);
    let (outputs, modes) = super::present::window_randr((1000, 700), (300, 210), (8192, 8192), 60);
    assert_eq!(outputs[0].mode_id, 3, "a size off the list");
    assert_eq!((outputs[0].mm_width, outputs[0].mm_height), (300, 210));
    assert_eq!(modes.len(), 9);
}

/// Rootless (plan RL0 Task 7): after an enter on a top-level, a host leave
/// moves the pointer to the root, so the client gets LeaveNotify with detail
/// NotifyAncestor (protocol §11: from T to its parent, the root).
/// Mutation: skip the motion after the scope change.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_leave_sends_leave_notify() {
    use yserver_core::{
        core_loop::InputOrigin,
        resources::ROOT_WINDOW,
        rootless::{self, CoordinatePolicy, RootlessInput},
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, ResourceId, SequenceNumber};

    const T: u32 = 0x0020_0001;
    const ENTER_LEAVE: u32 = 0x10 | 0x20;
    const LEAVE_NOTIFY: u8 = 8;
    const NOTIFY_ANCESTOR: u8 = 0;

    let Some(mut f) = input_or_skip(0) else {
        return;
    };
    yserver_core::backend::install_backend_root_bindings(&mut f.state, &f.b);
    rootless::enable(&mut f.state, &mut f.b, CoordinatePolicy::Opaque).expect("enable");
    let mut sequence = 0;
    let mut request = |f: &mut InputFixture, opcode: u8, data: u8, body: &[u8]| {
        sequence += 1;
        yserver_core::core_loop::process_request::process_request(
            &mut f.state,
            &mut f.b as &mut dyn Backend,
            ClientId(9),
            SequenceNumber(sequence),
            RequestHeader {
                opcode,
                data,
                length_units: u32::try_from(1 + body.len() / 4).expect("length"),
            },
            body,
            None,
        )
        .expect("process_request");
    };
    // CreateWindow T (10,10) 20x20, InputOutput, CWEventMask Enter|Leave.
    let create = [
        T.to_le_bytes().as_slice(),
        &ROOT_WINDOW.0.to_le_bytes(),
        &10i16.to_le_bytes(),
        &10i16.to_le_bytes(),
        &20u16.to_le_bytes(),
        &20u16.to_le_bytes(),
        &0u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0x800u32.to_le_bytes(),
        &ENTER_LEAVE.to_le_bytes(),
    ]
    .concat();
    request(&mut f, 1, 24, &create);
    request(&mut f, 8, 0, &T.to_le_bytes());
    rootless::reconcile_and_dispatch(&mut f.state, &mut f.b);
    let id = f
        .state
        .rootless
        .as_ref()
        .and_then(|r| r.toplevel_id(ResourceId(T)))
        .expect("T is a top-level");
    let origin = InputOrigin::Physical(pointer_source());
    f.send(vec![HostInputEvent::Rootless(
        RootlessInput::PointerEnter {
            origin,
            toplevel: id,
            local: (5.0, 5.0),
            time: 1,
        },
    )]);
    f.received();
    f.send(vec![HostInputEvent::Rootless(
        RootlessInput::PointerLeave { origin, time: 2 },
    )]);
    let events = f.received();
    assert!(
        events
            .iter()
            .any(|(ty, detail, _)| (*ty, *detail) == (LEAVE_NOTIFY, NOTIFY_ANCESTOR)),
        "LeaveNotify(NotifyAncestor) on T: {events:?}"
    );
}

/// Rootless (review fix): deactivation releases each held key on the device
/// that pressed it, so that device's next press of the key is a press, not a
/// duplicate. Mutation: release as `NestedHost` (master only).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_deactivate_releases_keys_on_the_pressing_device() {
    use yserver_core::{
        core_loop::InputOrigin,
        host_x11::HostKeyEvent,
        rootless::{self, CoordinatePolicy, HostRequest},
    };

    const KEYCODE: u8 = 38;
    let Some(mut f) = input_or_skip(KEY_MASKS) else {
        return;
    };
    yserver_core::backend::install_backend_root_bindings(&mut f.state, &f.b);
    rootless::enable(&mut f.state, &mut f.b, CoordinatePolicy::Opaque).expect("enable");
    let press = |pressed: bool| {
        HostInputEvent::Key(HostKeyEvent {
            origin: InputOrigin::Physical(keyboard_source()),
            pressed,
            keycode: KEYCODE,
            time: 1,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        })
    };
    let kinds = |events: &[(u8, u8, u16)]| -> Vec<u8> {
        events
            .iter()
            .filter(|(_, detail, _)| *detail == KEYCODE)
            .map(|(ty, _, _)| *ty)
            .collect()
    };

    f.send(vec![press(true)]);
    assert_eq!(kinds(&f.received()), vec![KEY_PRESS]);
    rootless::host_request(&mut f.state, &mut f.b, HostRequest::Deactivated);
    assert_eq!(kinds(&f.received()), vec![KEY_RELEASE]);
    assert!(
        f.state
            .key_down_by_device
            .values()
            .chain(f.state.unpublished_keyboard_keys_down.values())
            .all(|keys| !keys.contains_key(&KEYCODE)),
        "the pressing device's record is cleared"
    );
    // Deactivation focused None; a client gives focus back (PointerRoot).
    yserver_core::core_loop::process_request::process_request(
        &mut f.state,
        &mut f.b as &mut dyn Backend,
        yserver_protocol::x11::ClientId(9),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 42,
            data: 1,
            length_units: 3,
        },
        &[1u32.to_le_bytes(), 0u32.to_le_bytes()].concat(),
        None,
    )
    .expect("SetInputFocus");
    f.received();
    f.send(vec![press(true)]);
    assert_eq!(kinds(&f.received()), vec![KEY_PRESS], "pressed again");
}

/// Plan RL1 Task 7b: window resizes with a frame between each retire the
/// old swapchains after their presents (present fences, maintenance1)
/// instead of a queue-idle per rebuild, and validation stays clean.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_resize_storm_is_validation_clean() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    for k in 1..=8u16 {
        w.host_resize((64 + 8 * k, 48 + 4 * k), None);
        w.frame();
    }
    let vk = Arc::clone(w.f.b.platform.outputs[0].vk());
    if vk.swapchain_maintenance1 {
        assert_eq!(
            w.f.b.platform.outputs[0].rebuild_waits, 0,
            "no queue-idle per rebuild"
        );
    }
    drop(w);
    if vk.validation_enabled() {
        assert_eq!(vk.validation_errors(), 0);
    } else {
        eprintln!("validation was off");
    }
}

/// A present that fails before it is queued leaves a fence that never
/// signals: the old swapchain must still retire (queue-idle), and
/// validation stays clean.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_present_failure_does_not_stall_retirement() {
    let Some(mut w) = window_or_skip() else {
        return;
    };
    w.frame();
    w.f.b.platform.outputs[0].fail_next_present = true;
    let xid = WindowHandle::from_raw(1).expect("root").as_raw();
    fill(&mut w.f.b, xid, 0x00ff_0000, (0, 0, 8, 8));
    w.frame();
    for k in 1..=3u16 {
        w.host_resize((64 + 8 * k, 48), None);
        fill(&mut w.f.b, xid, 0x0000_ff00, (0, 0, 8, 8));
        w.frame();
    }
    assert_eq!(
        w.f.b.platform.outputs[0].retired_count(),
        0,
        "retirement stalled"
    );
    let vk = Arc::clone(w.f.b.platform.outputs[0].vk());
    drop(w);
    if vk.validation_enabled() {
        assert_eq!(vk.validation_errors(), 0);
    } else {
        eprintln!("validation was off");
    }
}

/// Spec §5: rootless RandR configuration fails with a real status, and no
/// window is resized (there is none to resize).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_refuses_randr_configuration() {
    let vk = match VkContext::new_with_options(VkContextOptions {
        wants_present: false,
        surface_extension: None,
        wants_kms_interop: false,
    }) {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            return;
        }
    };
    let root = (2560, 1600);
    let mut b = headless_backend(vk, root, None).expect("headless backend");
    let ui = Arc::new(RecordingUi::default());
    b.pstate.ui = Some(Arc::clone(&ui) as Arc<dyn ShellUi>);
    let mut f = screen_fixture(b);
    crate::rootless::start(
        &mut f.state,
        &mut f.b,
        Box::new(NoFramesHost),
        yserver_core::rootless::CoordinatePolicy::Global { origin: (0, 0) },
    )
    .expect("rootless start");
    assert_eq!(f.set_crtc_config((1024, 768), 0, 1), Ok(SET_CONFIG_FAILED));
    assert!(
        !ui.take()
            .iter()
            .any(|c| matches!(c, UiCall::ContentSize(..))),
        "no window resize asked"
    );
    assert_eq!(f.b.platform.output_size, root);
    assert!(
        f.screen_size_error(3000, 2000).is_some(),
        "RRSetScreenSize fails"
    );
    assert_eq!(f.b.platform.fb_size(), root, "the root keeps its size");
    assert_eq!(
        (f.state.randr.screen_width, f.state.randr.screen_height),
        root
    );
}

/// Two top-levels for drag tests on the 64x48 root: A (2,2 20x20) and
/// override-redirect popup P (30,2 15x15), both selecting crossing, button and motion
/// events for client 9; their ids, and a request sender.
fn rootless_drag_fixture() -> Option<(
    InputFixture,
    yserver_core::rootless::ToplevelId,
    yserver_core::rootless::ToplevelId,
    Box<DragRequest>,
)> {
    use yserver_core::rootless::{self, CoordinatePolicy};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut f = input_or_skip(0)?;
    yserver_core::backend::install_backend_root_bindings(&mut f.state, &f.b);
    rootless::enable(&mut f.state, &mut f.b, CoordinatePolicy::Opaque).expect("enable");
    let mut sequence = 0u16;
    let mut request: Box<DragRequest> = Box::new(
        move |f: &mut InputFixture, opcode: u8, data: u8, body: &[u8]| {
            sequence += 1;
            yserver_core::core_loop::process_request::process_request(
                &mut f.state,
                &mut f.b as &mut dyn Backend,
                ClientId(9),
                SequenceNumber(sequence),
                RequestHeader {
                    opcode,
                    data,
                    length_units: u32::try_from(1 + body.len() / 4).expect("length"),
                },
                body,
                None,
            )
            .expect("process_request");
        },
    );
    let a = drag_toplevel(&mut f, &mut request, DRAG_A, (2, 2, 20), false);
    let p = drag_toplevel(&mut f, &mut request, DRAG_P, (30, 2, 15), true);
    Some((f, a, p, request))
}

/// Create and map top-level `wid` at `(x, y)`, `side` square, selecting
/// the drag events; its id.
fn drag_toplevel(
    f: &mut InputFixture,
    request: &mut DragRequest,
    wid: u32,
    (x, y, side): (i16, i16, u16),
    popup: bool,
) -> yserver_core::rootless::ToplevelId {
    use yserver_core::{resources::ROOT_WINDOW, rootless};
    use yserver_protocol::x11::ResourceId;
    // CWOverrideRedirect (bit 9) then CWEventMask (bit 11).
    let create = [
        wid.to_le_bytes().as_slice(),
        &ROOT_WINDOW.0.to_le_bytes(),
        &x.to_le_bytes(),
        &y.to_le_bytes(),
        &side.to_le_bytes(),
        &side.to_le_bytes(),
        &0u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0xA00u32.to_le_bytes(),
        &u32::from(popup).to_le_bytes(),
        &DRAG_EVENTS.to_le_bytes(),
    ]
    .concat();
    request(f, 1, 24, &create);
    request(f, 8, 0, &wid.to_le_bytes());
    rootless::reconcile_and_dispatch(&mut f.state, &mut f.b);
    f.state
        .rootless
        .as_ref()
        .and_then(|r| r.toplevel_id(ResourceId(wid)))
        .expect("a top-level")
}

/// Sends one request as client 9.
type DragRequest = dyn FnMut(&mut InputFixture, u8, u8, &[u8]);

const DRAG_A: u32 = 0x0020_0001;
const DRAG_P: u32 = 0x0020_0002;
const DRAG_P2: u32 = 0x0020_0003;
/// ButtonPress | ButtonRelease | EnterWindow | LeaveWindow | PointerMotion.
const DRAG_EVENTS: u32 = 0x4 | 0x8 | 0x10 | 0x20 | 0x40;

/// `(type, event window, event_x, event_y)` of each event the client read.
fn drag_events(f: &mut InputFixture) -> Vec<(u8, u32, i16, i16)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = f.peer.read(&mut buf) {
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    bytes
        .chunks_exact(32)
        .filter(|ev| ev[0] & 0x7f >= 2)
        .map(|ev| {
            let word = |i: usize| u32::from_le_bytes([ev[i], ev[i + 1], ev[i + 2], ev[i + 3]]);
            let half = |i: usize| i16::from_le_bytes([ev[i], ev[i + 1]]);
            (ev[0] & 0x7f, word(12), half(24), half(26))
        })
        .collect()
}

/// gtk3-demo's combo box: a press in A opens popup P, which grabs the
/// pointer (owner_events); the host reports the drag at the window under
/// the pointer, so P gets the motion and the release in its coordinates.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_drag_into_a_grabbing_popup_releases_there() {
    use yserver_core::{core_loop::InputOrigin, rootless::RootlessInput};
    const BUTTON_RELEASE: u8 = 5;
    const ENTER_NOTIFY: u8 = 7;

    let Some((mut f, a, p, mut request)) = rootless_drag_fixture() else {
        return;
    };
    let origin = InputOrigin::Physical(pointer_source());
    let button = |pressed| HostInputEvent::PointerButton {
        origin,
        button: 0x110,
        pressed,
        time: 3,
    };
    f.send(vec![
        HostInputEvent::Rootless(RootlessInput::PointerEnter {
            origin,
            toplevel: a,
            local: (5.0, 5.0),
            time: 1,
        }),
        button(true),
    ]);
    // GrabPointer(P, owner_events, the drag events, async, async).
    let grab = [
        DRAG_P.to_le_bytes().as_slice(),
        &u16::try_from(DRAG_EVENTS & 0xFFFF)
            .expect("mask")
            .to_le_bytes(),
        &[1, 1],
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
    ]
    .concat();
    request(&mut f, 26, 1, &grab);
    drag_events(&mut f);
    f.send(vec![
        HostInputEvent::Rootless(RootlessInput::PointerEnter {
            origin,
            toplevel: p,
            local: (4.0, 6.0),
            time: 4,
        }),
        HostInputEvent::Rootless(RootlessInput::PointerMotionLocal {
            origin,
            toplevel: p,
            local: (7.0, 8.0),
            time: 5,
        }),
        button(false),
    ]);
    let events = drag_events(&mut f);
    assert!(
        events
            .iter()
            .any(|&(ty, w, ..)| (ty, w) == (ENTER_NOTIFY, DRAG_P)),
        "EnterNotify on P: {events:?}"
    );
    assert!(
        events.contains(&(BUTTON_RELEASE, DRAG_P, 7, 8)),
        "the release in P's coordinates: {events:?}"
    );
}

/// Without a grab, the press's implicit grab keeps A: a drag out to P and
/// back releases in A, in A's coordinates.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_drag_back_into_the_pressed_window_releases_there() {
    use yserver_core::{core_loop::InputOrigin, rootless::RootlessInput};
    const BUTTON_RELEASE: u8 = 5;

    let Some((mut f, a, p, _request)) = rootless_drag_fixture() else {
        return;
    };
    let origin = InputOrigin::Physical(pointer_source());
    let enter = |toplevel, local| {
        HostInputEvent::Rootless(RootlessInput::PointerEnter {
            origin,
            toplevel,
            local,
            time: 2,
        })
    };
    let button = |pressed| HostInputEvent::PointerButton {
        origin,
        button: 0x110,
        pressed,
        time: 3,
    };
    f.send(vec![enter(a, (5.0, 5.0)), button(true)]);
    drag_events(&mut f);
    f.send(vec![
        enter(p, (4.0, 6.0)),
        enter(a, (6.0, 7.0)),
        button(false),
    ]);
    let events = drag_events(&mut f);
    assert!(
        events.contains(&(BUTTON_RELEASE, DRAG_A, 6, 7)),
        "the release in A: {events:?}"
    );
}

/// A menu bar: a press on A's title grabs on A (owner_events), the menu P
/// drops down, a drag into it opens submenu P2 on hover, and the release
/// over a P2 item reaches P2 in its coordinates.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_menubar_drag_into_a_submenu_releases_there() {
    use yserver_core::{core_loop::InputOrigin, rootless::RootlessInput};
    const BUTTON_RELEASE: u8 = 5;
    const ENTER_NOTIFY: u8 = 7;

    let Some((mut f, a, p, mut request)) = rootless_drag_fixture() else {
        return;
    };
    let origin = InputOrigin::Physical(pointer_source());
    let enter = |toplevel, local| {
        HostInputEvent::Rootless(RootlessInput::PointerEnter {
            origin,
            toplevel,
            local,
            time: 2,
        })
    };
    let button = |pressed| HostInputEvent::PointerButton {
        origin,
        button: 0x110,
        pressed,
        time: 3,
    };
    f.send(vec![enter(a, (5.0, 5.0)), button(true)]);
    let grab = [
        DRAG_A.to_le_bytes().as_slice(),
        &u16::try_from(DRAG_EVENTS & 0xFFFF)
            .expect("mask")
            .to_le_bytes(),
        &[1, 1],
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
    ]
    .concat();
    request(&mut f, 26, 1, &grab);
    f.send(vec![enter(p, (4.0, 6.0))]);
    // Hovering the item opens the submenu, mid-drag.
    let p2 = drag_toplevel(&mut f, &mut request, DRAG_P2, (46, 2, 15), true);
    drag_events(&mut f);
    f.send(vec![enter(p2, (9.0, 3.0)), button(false)]);
    let events = drag_events(&mut f);
    assert!(
        events
            .iter()
            .any(|&(ty, w, ..)| (ty, w) == (ENTER_NOTIFY, DRAG_P2)),
        "EnterNotify on P2: {events:?}"
    );
    assert!(
        events.contains(&(BUTTON_RELEASE, DRAG_P2, 9, 3)),
        "the release in P2's coordinates: {events:?}"
    );
}
