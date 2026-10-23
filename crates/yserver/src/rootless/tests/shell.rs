//! `rootless_backend`, the zero-output server a rootless window shell runs
//! (plan RL2 Task 8), on headless surfaces.

use std::sync::{Arc, atomic::AtomicUsize};

use yserver_core::{backend::Backend as _, resources::ROOT_WINDOW, rootless::CoordinatePolicy};

use super::{Call, Fixture, HandoffKind};
use crate::{
    kms::{
        render::{Platform as _, XBackend},
        vk::device::{VkContext, VkContextOptions},
    },
    surface::{
        SurfacePlatform,
        clock::FrameClock,
        link::ShellLink,
        server::{RootlessSpec, rootless_backend},
        ui::{CursorImage, ShellUi},
    },
};

struct NoUi;

impl ShellUi for NoUi {
    fn set_display(&self, _display: u16) {}
    fn quit(&self, _failed: bool) {}
    fn set_cursor(&self, _cursor: Option<CursorImage>) {}
    fn warp_pointer(&self, _x: i32, _y: i32) {}
    fn set_content_size(&self, _width: u16, _height: u16, _token: u64) {}
}

const ROOT: (u16, u16) = (300, 200);

fn shell_or_skip() -> Option<(XBackend<SurfacePlatform>, RootlessSpec)> {
    let ext = ash::ext::headless_surface::NAME;
    let vk = match VkContext::new_with_options(VkContextOptions {
        wants_present: true,
        surface_extension: Some(ext),
        wants_kms_interop: false,
    }) {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            return None;
        }
    };
    let link = Arc::new(ShellLink::default());
    let spec = RootlessSpec {
        surface_extension: ext,
        root: ROOT,
        clock: Arc::new(FrameClock::new(Arc::clone(&link))),
        link,
        ui: Arc::new(NoUi),
        clients: Arc::new(AtomicUsize::new(0)),
    };
    let b = rootless_backend(vk, &spec, None).expect("rootless backend");
    Some((b, spec))
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_backend_has_no_outputs_a_clock_and_the_root_size() {
    let Some((mut b, spec)) = shell_or_skip() else {
        return;
    };
    assert!(b.platform.output_layouts().is_empty(), "no output");
    assert_eq!(b.platform.fb_size(), ROOT);
    let clock = b.platform.msc_clock().expect("a clock");
    assert!(std::ptr::eq(clock, Arc::as_ptr(&spec.clock)), "the spec's");
    spec.clock.set_refresh_mhz(120_000);
    let (outputs, modes) = b.randr_outputs_and_modes();
    let [output] = outputs.as_slice() else {
        panic!("one RandR output: {outputs:?}");
    };
    assert_eq!(output.name, "WINDOW-0");
    assert_eq!((output.x, output.y), (0, 0));
    assert_eq!((output.width, output.height), ROOT);
    assert_eq!(output.vrefresh, 120, "the clock's rate");
    assert_eq!((modes[0].width, modes[0].height), ROOT, "preferred");
    assert!(b.pstate.link.is_some() && b.pstate.ui.is_some());
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rootless_backend_presents_a_mapped_window() {
    let Some((b, spec)) = shell_or_skip() else {
        return;
    };
    let mut f = Fixture::with_backend(b, Arc::clone(&spec.link));
    f.install_with(CoordinatePolicy::Global { origin: (0, 0) }, |log| {
        log.handoff = HandoffKind::VulkanSurface;
    });
    const W: u32 = 0x0010_0001;
    f.create_window(W, ROOT_WINDOW.0, (10, 10, 40, 30), 0);
    f.map(W);
    f.fill(W, 0x00ff_0000, (0, 0, 40, 30));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while f.presenter().progress < 1 {
        assert!(std::time::Instant::now() < deadline, "{:?}", f.calls());
        f.tail();
    }
    let id = f.id(W);
    assert!(f.calls().contains(&Call::FirstFrame(id)));
    f.assert_validation_clean();
}
