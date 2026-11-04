//! GPU frames through per-window swapchains on `VK_EXT_headless_surface`
//! (lavapipe). Every test ends with zero validation errors when the layer
//! is on (plan RL1 Task 7a).

use std::time::{Duration, Instant};

use ash::vk;
use yserver_core::{backend::Backend as _, resources::ROOT_WINDOW, rootless::RootlessEvent};

use super::{Call, Fixture, Setup, lock};
use crate::{
    kms::render::scene::OutputSink as _,
    rootless::{
        Path,
        host::FrameEvent,
        window_swapchain::{Capture, QUEUE_IDLES, WindowSwapchain},
    },
};

const W: u32 = 0x0010_0001;

fn mapped(f: &mut Fixture) {
    f.create_window(W, ROOT_WINDOW.0, (10, 10, 40, 30), 2);
    f.map(W);
}

fn progress(f: &mut Fixture) -> u64 {
    f.presenter().progress
}

/// Tail until the presenter showed `n` frames in all (2 s at most).
fn shown_until(f: &mut Fixture, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while progress(f) < n {
        assert!(
            Instant::now() < deadline,
            "{} of {n} frames: {:?}",
            progress(f),
            f.calls()
        );
        f.tail();
        std::thread::yield_now();
    }
}

/// Tail (and run `before_block`) until nothing retires (2 s at most).
fn retired(f: &mut Fixture) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !f.presenter().retiring.is_empty() {
        assert!(Instant::now() < deadline, "retirement stalled");
        f.tail();
        f.b.before_block();
        std::thread::yield_now();
    }
}

fn with_swapchain<R>(
    f: &mut Fixture,
    id: yserver_core::rootless::ToplevelId,
    run: impl FnOnce(&mut WindowSwapchain) -> R,
) -> R {
    match &mut f.presenter().windows.get_mut(&id).expect("window").path {
        Path::Gpu(gpu) => run(gpu.swapchain.as_mut().expect("swapchain")),
        Path::Cpu(_) => panic!("a GPU window"),
    }
}

fn copy_fence(f: &mut Fixture) -> vk::Fence {
    let id = f.id(W);
    match &f.presenter().windows[&id].path {
        Path::Gpu(gpu) => gpu
            .swapchain
            .as_ref()
            .and_then(|sc| sc.last_copy.as_ref())
            .map(|t| t.fence())
            .expect("a copy"),
        Path::Cpu(_) => panic!("a GPU window"),
    }
}

/// §4.5.1 GPU row: `SetupDone` alone presents the first frame in the same
/// tail, and that present shows the window.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn setup_done_presents_the_first_frame_in_the_same_tail() {
    let Some(mut f) = Fixture::gpu_or_skip(|log| {
        log.setup = Setup::Withheld;
        log.ready = super::Ready::Manual;
    }) else {
        return;
    };
    mapped(&mut f);
    f.tail();
    let id = f.id(W);
    assert_eq!(progress(&mut f), 0);
    f.link.post_frame(FrameEvent::SetupDone { id });
    f.tail();
    assert_eq!(progress(&mut f), 1, "presented in the SetupDone tail");
    assert!(f.calls().contains(&Call::FirstFrame(id)));
    f.assert_validation_clean();
}

/// The image the present shows holds the whole backing: read back from the
/// acquired swapchain image after the copy, in the submitted command buffer.
/// Oracle: `get_image` of the backing.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn presented_image_holds_the_backing() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    f.fill(W, 0x00ff_0000, (3, 3, 10, 7));
    shown_until(&mut f, 1);
    let id = f.id(W);
    let vk = f.b.platform.gpu().vk().expect("vk").clone();
    if !with_swapchain(&mut f, id, WindowSwapchain::capture_next) {
        eprintln!("skipping: the surface makes no TRANSFER_SRC images");
        return;
    }
    f.fill(W, 0x0000_ff00, (20, 10, 9, 9));
    shown_until(&mut f, 2);
    let captured = with_swapchain(&mut f, id, |sc| {
        sc.last_copy
            .as_ref()
            .expect("a copy")
            .wait(&vk)
            .expect("copy done");
        sc.capture.as_ref().and_then(Capture::bytes)
    })
    .expect("captured the presented image");
    assert_eq!(captured, f.full_readback(W));
    f.assert_validation_clean();
}

/// A host copy scale of 2 (Windows at 200%) shows each backing pixel as
/// 2x2 swapchain pixels: the headless surface takes the extent asked for,
/// so the image is twice the backing and holds it blown up. Oracle:
/// `get_image` of the backing, each pixel repeated.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_copy_scale_of_two_shows_each_backing_pixel_as_two_by_two() {
    let Some(mut f) = Fixture::gpu_or_skip(|log| log.copy_scale = 2) else {
        return;
    };
    mapped(&mut f);
    f.fill(W, 0x00ff_0000, (3, 3, 10, 7));
    shown_until(&mut f, 1);
    let id = f.id(W);
    let vk = f.b.platform.gpu().vk().expect("vk").clone();
    if !with_swapchain(&mut f, id, WindowSwapchain::capture_next) {
        eprintln!("skipping: the surface makes no TRANSFER_SRC images");
        return;
    }
    f.fill(W, 0x0000_ff00, (20, 10, 9, 9));
    shown_until(&mut f, 2);
    let captured = with_swapchain(&mut f, id, |sc| {
        sc.last_copy
            .as_ref()
            .expect("a copy")
            .wait(&vk)
            .expect("copy done");
        sc.capture.as_ref().and_then(Capture::bytes)
    })
    .expect("captured the presented image");
    // The 40x30 window has a 2-pixel border: a 44x34 backing.
    let backing = f.full_readback(W);
    let (w, h) = (44, 34);
    assert_eq!(backing.len(), w * h * 4);
    let mut expected = Vec::with_capacity(backing.len() * 4);
    for row in backing.chunks_exact(w * 4) {
        let wide: Vec<u8> = row
            .chunks_exact(4)
            .flat_map(|px| px.iter().chain(px).copied().collect::<Vec<u8>>())
            .collect();
        expected.extend_from_slice(&wide);
        expected.extend_from_slice(&wide);
    }
    assert_eq!(captured, expected);
    f.assert_validation_clean();
}

/// §4.5: the GPU path acks after a successful present only; a failed one
/// leaves the damage in the store.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gpu_ack_after_present() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let id = f.id(W);
    if let Path::Gpu(gpu) = &mut f.presenter().windows.get_mut(&id).expect("W").path {
        gpu.swapchain.as_mut().expect("swapchain").fail_next_submit = true;
    }
    f.fill(W, 0x0000_ff00, (0, 0, 6, 6));
    assert!(!f.store_damage(W).is_empty());
    for _ in 0..2 {
        f.tail();
    }
    assert_eq!(progress(&mut f), 1, "the failed present showed nothing");
    assert!(
        !f.store_damage(W).is_empty(),
        "not acked on a failed present"
    );
    shown_until(&mut f, 2);
    assert!(f.store_damage(W).is_empty(), "acked once presented");
    f.assert_validation_clean();
}

/// The copy's ticket is the backing's render ticket, so a backing freed by
/// a resize (or a destroy) right after the present outlives the copy.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn gpu_copy_keeps_the_backing_alive() {
    for destroy in [false, true] {
        let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
            return;
        };
        mapped(&mut f);
        shown_until(&mut f, 1);
        let old = f.backing(W);
        let copy = copy_fence(&mut f);
        let ticket =
            f.b.store
                .get(old)
                .and_then(|d| d.last_render_ticket.as_ref())
                .map(|t| t.fence());
        assert_eq!(
            ticket,
            Some(copy),
            "the copy is the backing's render ticket"
        );
        if destroy {
            f.unmap(W);
            f.request(4, 0, &W.to_le_bytes());
        } else {
            f.resize(W, 60, 40);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while f.b.store.get(old).is_some() {
            assert!(Instant::now() < deadline, "the old backing never retired");
            f.tail();
            f.b.before_block();
            std::thread::yield_now();
        }
        retired(&mut f);
        f.assert_validation_clean();
    }
}

/// Unmap retires the swapchain; the surface goes (`surface_released`) only
/// after the host heard `Unmapped`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn unmap_retires_instead_of_destroying() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let id = f.id(W);
    let idles = QUEUE_IDLES.with(std::cell::Cell::get);
    f.unmap(W);
    yserver_core::rootless::reconcile_and_dispatch(&mut f.state, &mut f.b);
    assert_eq!(f.presenter().retiring.len(), 1, "retired, not destroyed");
    assert!(!f.calls().contains(&Call::SurfaceReleased(id)));
    retired(&mut f);
    let calls = f.calls();
    let unmapped = calls
        .iter()
        .position(|c| *c == Call::Events(vec![RootlessEvent::Unmapped { id }]))
        .expect("Unmapped");
    let released = calls
        .iter()
        .position(|c| *c == Call::SurfaceReleased(id))
        .expect("surface released");
    assert!(unmapped < released);
    if f.b.platform.gpu().vk().expect("vk").swapchain_maintenance1 {
        assert_eq!(
            QUEUE_IDLES.with(std::cell::Cell::get),
            idles,
            "present fences, no wait"
        );
    }
    f.assert_validation_clean();
}

/// Without maintenance1, retirement is one queue-idle, in the same tail.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fallback_without_maintenance1_waits_queue_idle_once() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    f.presenter().no_maintenance1 = true;
    mapped(&mut f);
    shown_until(&mut f, 1);
    f.resize(W, 50, 50);
    shown_until(&mut f, 2);
    let id = f.id(W);
    retired(&mut f);
    let idles = QUEUE_IDLES.with(std::cell::Cell::get);
    f.unmap(W);
    f.tail();
    assert!(f.presenter().retiring.is_empty(), "gone in the unmap tail");
    assert_eq!(
        QUEUE_IDLES.with(std::cell::Cell::get),
        idles + 1,
        "one queue-idle"
    );
    assert!(f.calls().contains(&Call::SurfaceReleased(id)));
    f.assert_validation_clean();
}

/// Old swapchains pass through retirement and go once their presents are
/// done; validation stays clean (acquire semaphore reuse, destroying a
/// swapchain in use).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rapid_resizes_retire_old_swapchains_cleanly() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let mut retirements = 0;
    for k in 1..=8u16 {
        f.resize(W, 40 + k * 3, 30 + k * 2);
        let before = f.presenter().retiring.len();
        shown_until(&mut f, u64::from(k) + 1);
        retirements += usize::from(f.presenter().retiring.len() > before);
    }
    assert!(retirements > 0, "old swapchains went through retiring");
    retired(&mut f);
    f.assert_validation_clean();
}

/// A failed copy submit leaves no acquire semaphore pending; later frames
/// present cleanly.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn failed_copy_submit_does_not_reuse_the_acquire_semaphore() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let id = f.id(W);
    let before = with_swapchain(&mut f, id, |sc| sc.handle());
    if let Path::Gpu(gpu) = &mut f.presenter().windows.get_mut(&id).expect("W").path {
        gpu.swapchain.as_mut().expect("swapchain").fail_next_submit = true;
    }
    f.fill(W, 0x0000_00ff, (0, 0, 4, 4));
    for _ in 0..3 {
        f.tail();
    }
    for k in 0..20u16 {
        f.fill(W, 0x0001_0000 * u32::from(k + 1), (k as i16, 0, 2, 2));
        let n = progress(&mut f) + 1;
        shown_until(&mut f, n);
    }
    assert_ne!(
        with_swapchain(&mut f, id, |sc| sc.handle()),
        before,
        "an abandoned image rebuilds the swapchain, never vkReleaseSwapchainImagesEXT"
    );
    lock(&f.log).calls.clear();
    f.assert_validation_clean();
}

/// A present that fails before it is queued (its fence never signals) does
/// not stall the swapchain's retirement, and later frames present cleanly.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_failure_retires_without_stalling() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let id = f.id(W);
    if let Path::Gpu(gpu) = &mut f.presenter().windows.get_mut(&id).expect("W").path {
        gpu.swapchain.as_mut().expect("swapchain").fail_next_present = true;
    }
    f.fill(W, 0x0000_00ff, (0, 0, 4, 4));
    for _ in 0..3 {
        f.tail();
    }
    for k in 0..5u16 {
        f.fill(W, 0x0001_0000 * u32::from(k + 1), (k as i16, 0, 2, 2));
        let n = progress(&mut f) + 1;
        shown_until(&mut f, n);
    }
    retired(&mut f);
    f.unmap(W);
    f.tail();
    retired(&mut f);
    f.assert_validation_clean();
}

/// An allocation failure after the acquire (copy ticket, present fence)
/// gives the image back and leaves the acquire semaphore consumed: later
/// frames present, retirement completes, validation stays clean.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn allocation_failure_after_acquire_abandons_the_image() {
    for no_maintenance1 in [false, true] {
        let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
            return;
        };
        f.presenter().no_maintenance1 = no_maintenance1;
        mapped(&mut f);
        shown_until(&mut f, 1);
        let id = f.id(W);
        for which in 0..2 {
            if let Path::Gpu(gpu) = &mut f.presenter().windows.get_mut(&id).expect("W").path {
                let sc = gpu.swapchain.as_mut().expect("swapchain");
                if which == 0 {
                    sc.fail_next_ticket = true;
                } else {
                    sc.fail_next_fence = true;
                }
            }
            f.fill(W, 0x0000_00ff, (0, 0, 4, 4));
            for _ in 0..3 {
                f.tail();
            }
            for k in 0..6u16 {
                f.fill(W, 0x0001_0000 * u32::from(k + 1), (k as i16, 0, 2, 2));
                let n = progress(&mut f) + 1;
                shown_until(&mut f, n);
            }
        }
        f.unmap(W);
        f.tail();
        retired(&mut f);
        f.assert_validation_clean();
    }
}

/// The host hears of every swapchain (re)build before it and after it,
/// at the native content extent (§4.0: outer = content + 2·bw).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn rebuild_hooks_bracket_every_swapchain_change() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let id = f.id(W);
    let hooks = |f: &Fixture| {
        f.calls()
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    Call::Resizing(..) | Call::Rebuilt(_) | Call::FirstFrame(_)
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        hooks(&f),
        [
            Call::Resizing(id, (44, 34)),
            Call::FirstFrame(id),
            Call::Rebuilt(id)
        ],
        "resizing, the present, rebuilt"
    );
    lock(&f.log).calls.clear();
    f.resize(W, 60, 40);
    f.fill(W, 0x00ff_0000, (0, 0, 60, 40));
    shown_until(&mut f, 2);
    assert_eq!(hooks(&f), [Call::Resizing(id, (64, 44)), Call::Rebuilt(id)]);
    lock(&f.log).calls.clear();
    f.fill(W, 0x0000_ff00, (0, 0, 4, 4));
    shown_until(&mut f, 3);
    assert_eq!(hooks(&f), [], "a damage frame rebuilds nothing");
    f.assert_validation_clean();
}

/// A surface that stays out of date is retried on a backoff, not in a
/// busy loop; the first frame after it recovers is shown and resets it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn out_of_date_surface_backs_off() {
    let Some(mut f) = Fixture::gpu_or_skip(|_| {}) else {
        return;
    };
    mapped(&mut f);
    shown_until(&mut f, 1);
    let id = f.id(W);
    with_swapchain(&mut f, id, |sc| sc.always_out_of_date = true);
    f.fill(W, 0x0000_ff00, (0, 0, 4, 4));
    // The core loop: a tail whenever the presenter's deadline is due, or
    // (no deadline) when the host's link would wake it.
    let start = Instant::now();
    let window = Duration::from_millis(300);
    let mut tails = 0;
    while start.elapsed() < window {
        let now = Instant::now();
        match f.wakeups(now).0 {
            Some(due) if due > now => {
                std::thread::sleep((due - now).min(window.saturating_sub(start.elapsed())));
            }
            _ => {
                f.tail();
                tails += 1;
            }
        }
        assert!(
            tails < 100,
            "busy loop: {tails} tails, {} attempts, {:?}",
            with_swapchain(&mut f, id, |sc| sc.out_of_date_count),
            f.calls().len()
        );
    }
    let attempts = with_swapchain(&mut f, id, |sc| sc.out_of_date_count);
    // 16 + 32 + 64 + 128 ms fit 300 ms: about five attempts, not hundreds.
    assert!((3..=8).contains(&attempts), "{attempts} attempts in 300 ms");
    with_swapchain(&mut f, id, |sc| sc.always_out_of_date = false);
    shown_until(&mut f, 2);
    f.assert_validation_clean();
}
