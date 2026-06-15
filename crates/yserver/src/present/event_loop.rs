//! Single-thread poll loop driving the painter, page-flips, and input.
//!
//! Always-animating: every page-flip completion advances the painter
//! state by `dt = now - last_paint`, paints the next free buffer, and
//! submits a new flip. CPU is proportional to refresh rate, not input.
//!
//! `running` is the shared shutdown flag. Step 11 uses it as the
//! polling exit condition (signal_hook flips it on SIGINT/SIGTERM);
//! Step 12 layers a signal watcher into the poll set as the primary
//! shutdown signal with the flag retained as backup.

use std::{
    io,
    os::fd::{AsFd, AsRawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[cfg(target_os = "freebsd")]
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
#[cfg(target_os = "linux")]
use nix::sys::{
    epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout},
    signalfd::SignalFd,
};

use crate::{
    drm::{self, Device, Swapchain},
    input::{self, InputEvent},
    present::{self, State},
};

const DRM_TOKEN: u64 = 1;
const INPUT_TOKEN: u64 = 2;
const SIGNAL_TOKEN: u64 = 3;

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub fn run_loop(
    device: &Arc<Device>,
    output: &drm::modeset::Output,
    swapchain: &mut Swapchain,
    input_ctx: Option<&mut input::Context>,
    signal_fd: &SignalFd,
    state: &mut State,
    fb_w: u16,
    fb_h: u16,
    running: &Arc<AtomicBool>,
) -> io::Result<()> {
    use std::os::fd::BorrowedFd;
    let epoll = Epoll::new(EpollCreateFlags::empty())?;

    let drm_borrow = unsafe { BorrowedFd::borrow_raw(device.as_fd().as_raw_fd()) };
    epoll.add(drm_borrow, EpollEvent::new(EpollFlags::EPOLLIN, DRM_TOKEN))?;

    if let Some(ctx) = input_ctx.as_ref() {
        let input_borrow = unsafe { BorrowedFd::borrow_raw(ctx.fd()) };
        epoll.add(
            input_borrow,
            EpollEvent::new(EpollFlags::EPOLLIN, INPUT_TOKEN),
        )?;
    }

    epoll.add(
        signal_fd,
        EpollEvent::new(EpollFlags::EPOLLIN, SIGNAL_TOKEN),
    )?;

    let mut input_ctx = input_ctx;
    let mut events_buf = [EpollEvent::empty(); 4];
    let mut last_paint = Instant::now();
    let mut completed_flips: u64 = 0;
    let mut input_events: u64 = 0;
    let mut pending_input: Vec<InputEvent> = Vec::new();

    while running.load(Ordering::Acquire) {
        let n = match epoll.wait(&mut events_buf, EpollTimeout::NONE) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => return Err(io::Error::other(format!("epoll_wait: {err}"))),
        };

        for ev in &events_buf[..n] {
            match ev.data() {
                INPUT_TOKEN => {
                    if let Some(ctx) = input_ctx.as_deref_mut() {
                        for e in ctx.dispatch()? {
                            input_events += 1;
                            pending_input.push(e);
                        }
                    }
                }
                DRM_TOKEN => {
                    let mut handled = 0u32;
                    drm::page_flip::drain_events(device, |_crtc| handled += 1)?;
                    for _ in 0..handled {
                        if let Some(idx) = swapchain.submitted_idx() {
                            swapchain.complete(idx).map_err(|e| {
                                io::Error::other(format!("swapchain.complete: {e}"))
                            })?;
                            completed_flips += 1;
                        }
                    }
                    if handled > 0 {
                        let now = Instant::now();
                        let dt = now.duration_since(last_paint).as_secs_f32();
                        last_paint = now;
                        present::update(state, dt, &pending_input, fb_w, fb_h);
                        pending_input.clear();

                        if let Some(idx) = swapchain.acquire_idx() {
                            present::paint(state, swapchain.buffer_mut(idx));
                            let fb_id = swapchain.buffer(idx).fb_id();
                            drm::page_flip::submit_flip(device, output, fb_id)?;
                            swapchain
                                .submit(idx)
                                .map_err(|e| io::Error::other(format!("swapchain.submit: {e}")))?;
                        }
                    }
                }
                SIGNAL_TOKEN => match signal_fd.read_signal() {
                    Ok(Some(siginfo)) => {
                        log::info!(
                            "yserver: received signal {} via signalfd — exiting loop",
                            siginfo.ssi_signo
                        );
                        running.store(false, Ordering::Release);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!("yserver: signalfd read failed: {err}");
                    }
                },
                token => {
                    log::warn!("epoll: unexpected token {token}");
                }
            }
        }
    }

    log::info!("yserver: loop exit — {completed_flips} flips, {input_events} input events");
    Ok(())
}

#[cfg(target_os = "freebsd")]
#[allow(clippy::too_many_arguments)]
pub fn run_loop(
    device: &Arc<Device>,
    output: &drm::modeset::Output,
    swapchain: &mut Swapchain,
    input_ctx: Option<&mut input::Context>,
    signal_kq: &Kqueue,
    state: &mut State,
    fb_w: u16,
    fb_h: u16,
    running: &Arc<AtomicBool>,
) -> io::Result<()> {
    let kq = Kqueue::new().map_err(|err| io::Error::other(format!("kqueue: {err}")))?;

    let drm_fd = device.as_fd().as_raw_fd();
    let mut changes: Vec<KEvent> = vec![KEvent::new(
        drm_fd as usize,
        EventFilter::EVFILT_READ,
        EvFlags::EV_ADD,
        FilterFlag::empty(),
        0,
        DRM_TOKEN as isize,
    )];

    if let Some(ctx) = input_ctx.as_ref() {
        changes.push(KEvent::new(
            ctx.fd() as usize,
            EventFilter::EVFILT_READ,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            0,
            INPUT_TOKEN as isize,
        ));
    }

    // Register the signal kqueue fd for readability (signals arrive there).
    changes.push(KEvent::new(
        signal_kq.as_fd().as_raw_fd() as usize,
        EventFilter::EVFILT_READ,
        EvFlags::EV_ADD,
        FilterFlag::empty(),
        0,
        SIGNAL_TOKEN as isize,
    ));

    let mut out = Vec::new();
    kq.kevent(
        &changes,
        &mut out,
        Some(libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    )
    .map_err(|err| io::Error::other(format!("kevent register: {err}")))?;

    let mut input_ctx = input_ctx;
    let mut kq_buf: Vec<KEvent> = vec![
        KEvent::new(
            0,
            EventFilter::EVFILT_READ,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0isize
        );
        4
    ];
    let mut last_paint = Instant::now();
    let mut completed_flips: u64 = 0;
    let mut input_events: u64 = 0;
    let mut pending_input: Vec<InputEvent> = Vec::new();

    while running.load(Ordering::Acquire) {
        let n = match kq.kevent(&[], &mut kq_buf, None) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => return Err(io::Error::other(format!("kevent: {err}"))),
        };

        for ev in &kq_buf[..n] {
            match ev.udata() as u64 {
                INPUT_TOKEN => {
                    if let Some(ctx) = input_ctx.as_deref_mut() {
                        for e in ctx.dispatch()? {
                            input_events += 1;
                            pending_input.push(e);
                        }
                    }
                }
                DRM_TOKEN => {
                    let mut handled = 0u32;
                    drm::page_flip::drain_events(device, |_crtc| handled += 1)?;
                    for _ in 0..handled {
                        if let Some(idx) = swapchain.submitted_idx() {
                            swapchain.complete(idx).map_err(|e| {
                                io::Error::other(format!("swapchain.complete: {e}"))
                            })?;
                            completed_flips += 1;
                        }
                    }
                    if handled > 0 {
                        let now = Instant::now();
                        let dt = now.duration_since(last_paint).as_secs_f32();
                        last_paint = now;
                        present::update(state, dt, &pending_input, fb_w, fb_h);
                        pending_input.clear();

                        if let Some(idx) = swapchain.acquire_idx() {
                            present::paint(state, swapchain.buffer_mut(idx));
                            let fb_id = swapchain.buffer(idx).fb_id();
                            drm::page_flip::submit_flip(device, output, fb_id)?;
                            swapchain
                                .submit(idx)
                                .map_err(|e| io::Error::other(format!("swapchain.submit: {e}")))?;
                        }
                    }
                }
                SIGNAL_TOKEN => {
                    // The signal kqueue became readable — drain it to find
                    // which signal fired. For this simple presenter loop we
                    // just set the shutdown flag.
                    let mut sig_events = [KEvent::new(
                        0,
                        EventFilter::EVFILT_SIGNAL,
                        EvFlags::empty(),
                        FilterFlag::empty(),
                        0,
                        0isize,
                    ); 4];
                    if let Ok(sn) = signal_kq.kevent(
                        &[],
                        &mut sig_events,
                        Some(libc::timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        }),
                    ) {
                        for sev in &sig_events[..sn] {
                            log::info!(
                                "yserver: received signal {} via kqueue — exiting loop",
                                sev.ident()
                            );
                            running.store(false, Ordering::Release);
                        }
                    }
                }
                token => {
                    log::warn!("kqueue: unexpected token {token}");
                }
            }
        }
    }

    log::info!("yserver: loop exit — {completed_flips} flips, {input_events} input events");
    Ok(())
}
