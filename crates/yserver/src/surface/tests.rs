//! The surface platform with no surfaces: sink behaviour without a GPU, and
//! the headless backend drawing on lavapipe.

use yserver_core::{
    backend::{Backend, WindowHandle},
    host_x11::HostSubwindowVisual,
};

use super::{SurfacePlatform, headless_backend};
use crate::kms::{
    render::{
        Platform,
        gpu::GpuCore,
        scene::{CursorPlaneSink, FrameTargetError, OutputRoute, OutputSink},
    },
    vk::device::{VkContext, VkContextOptions},
};

#[test]
fn surface_sink_without_surfaces_has_no_slots_and_no_cursor_plane() {
    let platform = SurfacePlatform::new(GpuCore::for_tests(), (640, 480));
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

fn headless_or_skip() -> Option<crate::kms::render::XBackend<SurfacePlatform>> {
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

    let rect = [2i16.to_le_bytes(), 2i16.to_le_bytes()]
        .concat()
        .into_iter()
        .chain(4u16.to_le_bytes())
        .chain(4u16.to_le_bytes())
        .collect::<Vec<u8>>();
    b.poly_fill_rectangle(None, xid, 0x00FF_0000, &rect)
        .expect("poly_fill_rectangle");
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
