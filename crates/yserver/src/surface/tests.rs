//! The surface platform: sink behaviour without a GPU, the headless backend
//! drawing on lavapipe, and a `VK_EXT_headless_surface` swapchain driven
//! through real compose → copy → present → retire cycles.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk;
use yserver_core::{
    backend::{Backend, BackendTopology, WindowHandle},
    host_x11::HostSubwindowVisual,
    server::ServerState,
};

use super::{
    SurfacePlatform,
    clock::FrameClock,
    headless_backend,
    link::{ShellEvent, ShellLink},
    surface_backend,
    swapchain::{NativeSurface, SurfaceOutput},
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
    assert!(!platform.fits(0, 16, 16), "the scene paints the cursor");
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
