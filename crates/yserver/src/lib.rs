pub mod clock;
pub mod drm;
pub mod input;
pub mod input_thread;
pub mod kms;
pub mod launch;
pub mod present;
mod seat;

use std::{fs, io, path::PathBuf, thread};

use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
#[cfg(target_os = "linux")]
use nix::sys::signalfd::SignalFd;

use yserver_core::{
    backend::Backend,
    core_loop::{self, Message, poll_tokens::ClientIdAllocator},
    resources::{ARGB_COLORMAP, ARGB_VISUAL, ROOT_VISUAL, ROOT_WINDOW},
    server::ServerState,
};

fn install_backend_root_bindings(state: &mut ServerState, backend: &dyn Backend) {
    if let Some(root) = state.resources.window_mut(ROOT_WINDOW) {
        root.host_xid = yserver_core::backend::WindowHandle::from_raw(backend.window_id());
    }
    state
        .resources
        .set_visual_host_xid(ROOT_VISUAL, backend.root_visual_xid());
    if let Some(host_colormap) = backend.argb_colormap_xid() {
        state
            .resources
            .set_colormap_host_xid(ARGB_COLORMAP, host_colormap);
    }
    if let Some(host_argb_visual) = backend.argb_visual_xid() {
        state
            .resources
            .set_visual_host_xid(ARGB_VISUAL, host_argb_visual);
    }
}

pub fn run(opts: launch::LaunchOptions) -> io::Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    panic!("yserver only supports Linux and FreeBSD (DRM/KMS, libinput, evdev)");

    log::info!("yserver: startup");

    // Capture the inherited SIGUSR1 disposition before signalfd masking.
    // If the DM started us with SIGUSR1 ignored, we signal it when ready.
    let sigusr1_was_ignored = launch::sigusr1_is_ignored();

    // Capture the parent (DM) PID now, before long init — if the parent
    // dies during startup and we get reparented, getppid() at readiness
    // would point at a subreaper or PID 1. Xorg captures it the same way.
    let parent_pid = launch::startup_parent_pid();

    // Vulkan-call-rate telemetry: emit a per-second snapshot of
    // call counters from `kms::vk::call_stats::VK_CALLS`. Gated on
    // the same `YSERVER_LOOP_TELEMETRY` env var the core-loop
    // telemetry uses so the two rollups appear together. The
    // counter increments at each call site are unconditional
    // (atomic-add is ~1ns); only the per-second emission is
    // env-gated.
    if std::env::var_os("YSERVER_LOOP_TELEMETRY").is_some() {
        thread::spawn(|| {
            use std::time::Duration;
            // Previous-snapshot cache for the pool delta. The pool's
            // stats counters are cumulative; we emit per-second
            // deltas so the line reads the same way as the vk-call
            // rates.
            let mut prev_pool = crate::kms::vk::pixmap_pool::PixmapPoolStats::default();
            loop {
                thread::sleep(Duration::from_secs(1));
                let s = crate::kms::vk::call_stats::VK_CALLS.snapshot_and_reset();
                log::info!(
                    "vk call rate [1s]: barrier2={} draw={} bind_pl={} bind_ds={} \
                     push_const={} viewport={} scissor={} begin_rendering={} \
                     end_rendering={} copy_b2i={} copy_i={} copy_i2b={} \
                     clear_color_image={} queue_submit2={} begin_cb={} end_cb={}",
                    s.cmd_pipeline_barrier2,
                    s.cmd_draw,
                    s.cmd_bind_pipeline,
                    s.cmd_bind_descriptor_sets,
                    s.cmd_push_constants,
                    s.cmd_set_viewport,
                    s.cmd_set_scissor,
                    s.cmd_begin_rendering,
                    s.cmd_end_rendering,
                    s.cmd_copy_buffer_to_image,
                    s.cmd_copy_image,
                    s.cmd_copy_image_to_buffer,
                    s.cmd_clear_color_image,
                    s.queue_submit2,
                    s.begin_command_buffer,
                    s.end_command_buffer,
                );
                // Submit attribution: which call sites drive
                // queue_submit2. Sum should approximately equal
                // queue_submit2 above (off by ≤ Idle-flush count from
                // the flush_if_needed pre-attribution).
                log::info!(
                    "vk submit src [1s]: vis_composite={} readback={} ext_sync={} \
                     protocol_barrier={} size_limit={} latency_limit={} shutdown={} \
                     one_shot={} compositor={} other={}",
                    s.submit_visible_composite,
                    s.submit_readback,
                    s.submit_external_sync,
                    s.submit_protocol_barrier,
                    s.submit_size_limit,
                    s.submit_latency_limit,
                    s.submit_shutdown,
                    s.submit_one_shot,
                    s.submit_compositor,
                    s.submit_other,
                );
                // ProtocolBarrier per-site breakdown — the sum of
                // these eight counters equals `protocol_barrier`
                // above. Identifies which lifecycle path drives the
                // ProtocolBarrier flush rate.
                log::info!(
                    "vk pb src [1s]: drawable_destroy={} window_resize={} \
                     image_dealloc_fb={} dmabuf_release={} picture_destroy={} \
                     cursor_picture={}",
                    s.pb_drawable_destroy,
                    s.pb_window_resize,
                    s.pb_image_dealloc_fallback,
                    s.pb_dmabuf_release,
                    s.pb_picture_destroy,
                    s.pb_cursor_picture,
                );
                // submit_other per-caller breakdown — sum equals
                // `other` above. Distinguishes cursor / window /
                // pixmap mirror init clears.
                log::info!(
                    "vk init_clear src [1s]: cursor={} window={} pixmap={}",
                    s.init_clear_cursor,
                    s.init_clear_window,
                    s.init_clear_pixmap,
                );
                // PixmapPool deltas — cumulative counters minus the
                // previous snapshot. Tells us per second whether the
                // pool is being consulted (takes_hit+takes_miss),
                // whether mirrors return to it (returns_accepted),
                // and which rejection path fires (bucket_full means
                // PIXMAP_POOL_BUCKET_CAP is too small; oversize
                // means MAX_POOLED_DIM is too small).
                if let Some(cur) = crate::kms::vk::pixmap_pool::telemetry_snapshot() {
                    let d_hit = cur.total_takes_hit.wrapping_sub(prev_pool.total_takes_hit);
                    let d_miss = cur
                        .total_takes_miss
                        .wrapping_sub(prev_pool.total_takes_miss);
                    let d_acc = cur
                        .total_returns_accepted
                        .wrapping_sub(prev_pool.total_returns_accepted);
                    let d_full = cur
                        .total_returns_rejected_bucket_full
                        .wrapping_sub(prev_pool.total_returns_rejected_bucket_full);
                    let d_over = cur
                        .total_returns_rejected_oversize
                        .wrapping_sub(prev_pool.total_returns_rejected_oversize);
                    // Per-bin oversize-reject breakdown by max(width, height).
                    // Bins match `pixmap_pool::OVERSIZE_BIN_THRESHOLDS`:
                    // `<=256`, `<=512`, `<=1024`, `>1024`.
                    let d_over_bins: [u64; 4] = std::array::from_fn(|i| {
                        cur.total_returns_rejected_oversize_by_bucket[i]
                            .wrapping_sub(prev_pool.total_returns_rejected_oversize_by_bucket[i])
                    });
                    log::info!(
                        "pixmap pool [1s]: takes_hit={} takes_miss={} \
                         returns_accepted={} returns_rejected_bucket_full={} \
                         returns_rejected_oversize={} \
                         returns_rejected_oversize_by_bin[<=256,<=512,<=1024,>1024]=[{},{},{},{}]",
                        d_hit,
                        d_miss,
                        d_acc,
                        d_full,
                        d_over,
                        d_over_bins[0],
                        d_over_bins[1],
                        d_over_bins[2],
                        d_over_bins[3],
                    );
                    prev_pool = cur;
                }
            }
        });
    }

    let signal_fd = block_termination_signals()?;

    // Take over the console TTY before opening anything else: stops the
    // kernel keyboard driver from delivering Ctrl-C / Ctrl-Z / etc. as
    // signals to the controlling TTY's foreground process group, which
    // would otherwise kill the whole session when the user hits Ctrl-C
    // inside an X client. Skipped silently when not on a Linux VC (pty
    // under SSH or a graphical terminal emulator).
    #[cfg(target_os = "linux")]
    let console_guard = crate::kms::console::ConsoleGuard::acquire(opts.vt)?;
    #[cfg(not(target_os = "linux"))]
    let console_guard: Option<()> = None;
    let device_path = resolve_drm_device()?;
    log::info!("yserver: opening DRM device {device_path}");

    // Open the seat first so DRM + input device opens can route through
    // it in libseat mode. Falls back to `Seat::Direct` silently when
    // libseat is unavailable (no seat manager / not on a real VT).
    let seat = crate::seat::Seat::open();

    // Build the backend in seat-aware fashion. In libseat mode the DRM
    // card is opened through the seat and libinput lives on the core
    // thread. In Direct mode today's behaviour is preserved exactly.
    let mut backend = build_kms_backend_v2(seat, &device_path, console_guard)?;
    let (fb_w, fb_h) = backend.fb_dimensions();
    log::info!("yserver: scanout {fb_w}x{fb_h}");

    let randr_outputs = backend.randr_outputs();
    let mut state = ServerState::with_randr_outputs(fb_w, fb_h, randr_outputs);
    // Tie the libinput thread's `clock::server_time_ms()` baseline
    // to ServerState's `start_instant` so the input-event timestamps
    // and the `state.timestamp_now()` clock used by the
    // UngrabPointer / AllowEvents / SetInputFocus time-check arms
    // share the same origin. Without this, the two `Instant`s were
    // initialised ~1.8 s apart (clock::START lazy-init on the input
    // thread's first dispatch, well after this point), and X clients
    // saw event timestamps drift behind `state.timestamp_now()` by
    // the same amount — wedging menu close paths that ungrab with
    // saved press timestamps.
    crate::clock::init(state.start_instant);
    state.dpms = yserver_core::server::DpmsState::new(backend.dpms_capable());
    state.glx_tfp_supported = backend.supports_dmabuf_export();
    install_backend_root_bindings(&mut state, &backend);

    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("create_dir_all({}): {e}", socket_dir.display()),
        )
    })?;
    let lock_dir = PathBuf::from("/tmp");

    // Resolve the effective display, acquire the lock (when -displayfd is
    // absent), and bind the socket. `_lock_guard` is held for the server's
    // lifetime; it drops at the end of `run()` — after the socket file is
    // removed at shutdown — so the lock (the authoritative occupancy
    // marker) outlives the socket. On any error after lock acquisition the
    // `?` unwinds and drops the guard, releasing the lock.
    let (display, listener, _lock_guard, socket_path) = match launch::resolve(&opts) {
        launch::Resolution::Explicit { display, lock } => {
            let guard = if lock {
                Some(launch::acquire_lock(&lock_dir, display)?)
            } else {
                None
            };
            let (listener, socket_path) = launch::bind_explicit(&socket_dir, display)?;
            (display, listener, guard, socket_path)
        }
        launch::Resolution::AutoPick => {
            let (display, listener, socket_path) = launch::autopick(&socket_dir)?;
            (display, listener, None, socket_path)
        }
    };
    log::info!("yserver: listening on unix socket DISPLAY=:{display}");

    // Initial composite+flip so the screen has a known frame before any
    // client connects.
    if let Err(e) = backend.composite_and_flip(&state) {
        log::warn!("yserver: initial composite_and_flip failed: {e}");
    }

    // Build the channel + waker before spawning anything: senders need
    // a clone, run_core needs the receiver.
    let (poll, sender, rx) = core_loop::channel()?;

    // Libseat mode: the backend owns libinput on the core thread.
    // Hand it the sender for Shutdown/Dump messages from the on-core
    // hotkey path. No input thread in this mode.
    //
    // Direct mode: spawn the dedicated libinput sender thread exactly
    // as before. After `take_input_ctx`, the backend's `poll_fds()`
    // returns only the DRM fd, so run_core's E3 registration step
    // won't double-poll libinput.
    if backend.is_libseat_mode() {
        backend.set_input_sender(sender.clone_handle());
        log::info!("yserver: libseat mode — libinput on core thread, no input thread spawned");
    } else if let Some(input_ctx) = backend.take_input_ctx() {
        let input_sender = sender.clone_handle();
        let input_control = std::sync::Arc::new(crate::input_thread::InputThreadControl::new()?);
        // Lock-LED relay: the core thread owns the XKB lock state, the
        // input thread owns the libinput devices; LED transitions
        // cross via this eventfd-backed mask.
        let led_relay = std::sync::Arc::new(crate::input::LedRelay::new()?);
        backend.set_input_thread_control(std::sync::Arc::clone(&input_control));
        backend.set_led_relay(std::sync::Arc::clone(&led_relay));
        log::info!("yserver: Direct mode — spawning libinput sender thread");
        thread::Builder::new()
            .name("yserver-libinput".into())
            .spawn(move || {
                if let Err(err) = input_thread::run(
                    input_ctx,
                    input_sender,
                    u32::from(fb_w),
                    u32::from(fb_h),
                    input_control,
                    led_relay,
                ) {
                    log::warn!("yserver: libinput thread exited: {err}");
                }
            })?;
    }

    // signalfd → Message bridge. yserver-core deliberately doesn't
    // depend on nix; a tiny thread wraps the SignalFd read so run_core
    // only sees channel-side messages. SIGINT/SIGTERM map to
    // `Shutdown`; SIGUSR1/SIGUSR2 map to VT release/acquire messages,
    // which fall back to diagnostic dumps when VT switching is not
    // armed.
    //
    // SIGUSR1 carries three distinct, non-conflicting meanings here:
    // (1) the *inherited disposition* read once at startup
    // (`sigusr1_was_ignored` above) drives the readiness handshake
    // *to the parent* DM (`launch::signal_ready`); (2) masked-and-
    // signalfd-consumed *delivery to self* triggers the scanout dump
    // below; (3) we *send* SIGUSR1 outward to the parent at readiness.
    // Disposition-in, delivery-to-self, and signal-out are separate.
    let signal_sender = sender.clone_handle();
    thread::Builder::new()
        .name("yserver-signalfd".into())
        .spawn(move || {
            let signal_fd = signal_fd;
            #[cfg(target_os = "linux")]
            loop {
                match signal_fd.read_signal() {
                    Ok(Some(siginfo)) => {
                        let signo = siginfo.ssi_signo as i32;
                        if signo == nix::libc::SIGUSR1 {
                            log::info!("yserver: received SIGUSR1, forwarding VT release");
                            if signal_sender.send(Message::VtRelease).is_err() {
                                return;
                            }
                            continue;
                        }
                        if signo == nix::libc::SIGUSR2 {
                            log::info!("yserver: received SIGUSR2, forwarding VT acquire");
                            if signal_sender.send(Message::VtAcquire).is_err() {
                                return;
                            }
                            continue;
                        }
                        log::info!("yserver: received signal {signo}, requesting shutdown");
                        let _ = signal_sender.send(Message::Shutdown);
                        return;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!("yserver: signalfd read error: {err}");
                        let _ = signal_sender.send(Message::Shutdown);
                        return;
                    }
                }
            }
            #[cfg(target_os = "freebsd")]
            {
                use nix::sys::event::KEvent;
                let mut events = [KEvent::new(
                    0,
                    nix::sys::event::EventFilter::EVFILT_SIGNAL,
                    nix::sys::event::EvFlags::empty(),
                    nix::sys::event::FilterFlag::empty(),
                    0,
                    0isize,
                ); 4];
                loop {
                    let n = match signal_fd.kevent(&[], &mut events, None) {
                        Ok(n) => n,
                        Err(nix::errno::Errno::EINTR) => continue,
                        Err(err) => {
                            log::warn!("yserver: kevent signal read error: {err}");
                            let _ = signal_sender.send(Message::Shutdown);
                            return;
                        }
                    };
                    for ev in &events[..n] {
                        let signo = ev.ident() as i32;
                        if signo == nix::libc::SIGUSR1 {
                            log::info!("yserver: received SIGUSR1, forwarding VT release");
                            if signal_sender.send(Message::VtRelease).is_err() {
                                return;
                            }
                            continue;
                        }
                        if signo == nix::libc::SIGUSR2 {
                            log::info!("yserver: received SIGUSR2, forwarding VT acquire");
                            if signal_sender.send(Message::VtAcquire).is_err() {
                                return;
                            }
                            continue;
                        }
                        log::info!("yserver: received signal {signo}, requesting shutdown");
                        let _ = signal_sender.send(Message::Shutdown);
                        return;
                    }
                }
            }
        })?;

    // Readiness handshake: ServerState is fully constructed, the socket is
    // bound + chmod'd, and the lock is held — we can complete an initial X
    // connection setup now. This is the analog of Xorg signaling after
    // CreateConnectionBlock() and before Dispatch().
    launch::signal_ready(&opts, display, sigusr1_was_ignored, parent_pid);

    let alloc = ClientIdAllocator::new();
    log::info!("yserver: entering single-threaded core loop");
    let result = core_loop::run_core(
        poll,
        rx,
        sender,
        &mut state,
        &mut backend,
        Some(listener),
        &alloc,
    );
    if let Err(err) = &result {
        log::warn!("yserver: run_core returned error: {err}");
    }

    log::info!("yserver: shutting down, disabling output");
    if let Err(e) = backend.disable_output() {
        log::warn!("yserver: disable_output failed: {e}");
    }
    // Stage 5 Task 6.1: fan out any PRESENT completions deferred past
    // shutdown drain — events must reach clients before we tear down
    // the socket.
    for entry in backend.take_shutdown_present_events() {
        yserver_core::core_loop::process_request::fire_present_completion_events(
            &mut state, &entry,
        );
    }

    // 2026-05-31: destroy every drawable's Vk handles before
    // `backend` drops, so `vkDestroyDevice` doesn't warn about
    // leaked `VkImage` / `VkImageView` / `VkDeviceMemory`.
    // `DrawableStore` has no `Drop` (`Storage::destroy` needs
    // `&PlatformBackend` for pool-return + DRI3-import handling
    // and Drop has no access to disjoint sibling fields), so
    // bridge them explicitly here.
    backend.shutdown_destroy_drawables();

    let _ = fs::remove_file(&socket_path);
    log::info!("yserver: master released, exiting");
    result
}

/// Build a `KmsBackendV2` in either libseat or Direct mode depending on
/// `seat`. This is the single decision point for the mode branch.
///
/// **Libseat mode** (`seat == Seat::Libseat`):
/// - Opens the DRM card through the seat (FATAL on failure — once libseat
///   has the session, direct device opens won't get DRM master).
/// - Builds a `crate::input::Context` on the core thread via
///   `Context::new_libseat` (FATAL on failure for the same reason).
/// - Returns a `KmsBackendV2` with `is_libseat_mode() == true`.
///
/// **Direct mode** (`seat == Seat::Direct`):
/// - Calls `KmsBackendV2::open(device_path, console_guard)` — the
///   direct-device path, with optional VT_PROCESS arming when a real
///   controlling console is present.
/// - Returns a `KmsBackendV2` with `is_libseat_mode() == false`.
fn build_kms_backend_v2(
    seat: crate::seat::Seat,
    device_path: &str,
    console_guard: crate::kms::ConsoleGuardOpt,
) -> io::Result<crate::kms::v2::KmsBackendV2> {
    match seat {
        crate::seat::Seat::Libseat { ref inner, .. } => {
            // Build on-core libinput first (before DRM open) so any failure
            // is reported clearly. If libinput fails here it is FATAL —
            // we're committed to libseat mode.
            let core_libinput = crate::input::Context::new_libseat(std::rc::Rc::clone(inner))
                .map_err(|e| {
                    io::Error::other(format!(
                        "libseat mode: building on-core libinput context failed: {e}"
                    ))
                })?;
            let core_libinput_fd = core_libinput.fd();
            let seat_fd = inner.borrow_mut().fd().map_err(|e| {
                io::Error::other(format!("libseat mode: getting seat fd failed: {e}"))
            })?;
            crate::kms::v2::KmsBackendV2::open_libseat(
                seat,
                device_path,
                console_guard,
                core_libinput,
                seat_fd,
                core_libinput_fd,
            )
        }
        crate::seat::Seat::Direct => {
            // Direct path: open DRM + libinput directly, optionally
            // arming VT_PROCESS if we have a controlling console.
            crate::kms::v2::KmsBackendV2::open(device_path, console_guard)
        }
    }
}

fn resolve_drm_device() -> io::Result<String> {
    if let Ok(explicit) = std::env::var("YSERVER_DRM_DEVICE") {
        return Ok(explicit);
    }
    // Split-driver systems expose a render-only card alongside the
    // KMS card. On Asahi (M1 / M2): `asahi` GPU is card0 (render-only,
    // MODE_GETRESOURCES → EOPNOTSUPP); `apple-drm` is card2 (KMS).
    // On AMD/Intel hybrid laptops similar layouts occur. The pre-asahi
    // resolver only probed card0/card1 and didn't distinguish render-
    // only nodes, so it would pick a card whose first KMS ioctl then
    // fails. Probe each /dev/dri/card* by attempting MODE_GETRESOURCES
    // (drm-rs's `resource_handles`) and keep the first that succeeds.
    use ::drm::control::Device as _;

    let mut entries: Vec<PathBuf> = match fs::read_dir("/dev/dri") {
        Ok(it) => it
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card"))
            })
            .collect(),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("read_dir(/dev/dri): {err}"),
            ));
        }
    };
    entries.sort();

    let mut reasons: Vec<String> = Vec::new();
    for path in entries {
        let path_str = match path.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let device = match drm::Device::open(&path_str) {
            Ok(d) => d,
            Err(err) => {
                log::info!("yserver: skipping {path_str}: open failed: {err}");
                reasons.push(format!("{path_str}: open failed: {err}"));
                continue;
            }
        };
        // Render-only drivers (asahi GPU, etc.) return EOPNOTSUPP here.
        // Anything else (success or some other error) we trust — let the
        // caller surface a downstream error rather than masking it.
        match device.resource_handles() {
            Ok(_) => return Ok(path_str),
            Err(err) => {
                log::info!("yserver: skipping {path_str}: not KMS-capable: {err}");
                reasons.push(format!("{path_str}: not KMS-capable: {err}"));
                // device drops here, releasing master.
            }
        }
    }
    Err(io::Error::other(format!(
        "no KMS-capable DRM device found under /dev/dri. Tried:\n  {}\n\
         Override with YSERVER_DRM_DEVICE=/dev/dri/cardN.",
        if reasons.is_empty() {
            "(no /dev/dri/card* entries)".to_string()
        } else {
            reasons.join("\n  ")
        }
    )))
}

#[cfg(target_os = "linux")]
fn block_termination_signals() -> io::Result<SignalFd> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    // SIGUSR1 → diagnostic scanout dump. Blocked so signalfd consumes
    // it instead of the default-action (which would terminate us).
    mask.add(Signal::SIGUSR1);
    // SIGUSR2 → diagnostic drawable-storage dump (root + COW + every
    // redirected backing). Same blocking rationale as SIGUSR1.
    mask.add(Signal::SIGUSR2);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;
    SignalFd::new(&mask).map_err(|err| io::Error::other(format!("signalfd: {err}")))
}

/// FreeBSD: block the same signals and return a kqueue fd with
/// EVFILT_SIGNAL filters registered.
#[cfg(target_os = "freebsd")]
fn block_termination_signals() -> io::Result<nix::sys::event::Kqueue> {
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGUSR1);
    mask.add(Signal::SIGUSR2);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;

    let kq = Kqueue::new().map_err(|err| io::Error::other(format!("kqueue: {err}")))?;
    let changes = [
        KEvent::new(
            libc::SIGINT as usize,
            EventFilter::EVFILT_SIGNAL,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            0,
            0isize,
        ),
        KEvent::new(
            libc::SIGTERM as usize,
            EventFilter::EVFILT_SIGNAL,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            0,
            0isize,
        ),
        KEvent::new(
            libc::SIGUSR1 as usize,
            EventFilter::EVFILT_SIGNAL,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            0,
            0isize,
        ),
        KEvent::new(
            libc::SIGUSR2 as usize,
            EventFilter::EVFILT_SIGNAL,
            EvFlags::EV_ADD,
            FilterFlag::empty(),
            0,
            0isize,
        ),
    ];
    let mut out = Vec::new();
    kq.kevent(&changes, &mut out, None)
        .map_err(|err| io::Error::other(format!("kevent register signals: {err}")))?;
    Ok(kq)
}

#[cfg(test)]
mod tests {
    use super::install_backend_root_bindings;
    use yserver_core::{
        backend::Backend,
        resources::{ARGB_COLORMAP, ARGB_VISUAL, ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };

    #[test]
    fn install_backend_root_bindings_sets_root_host_xid_and_visuals() {
        let mut state = ServerState::new();
        let backend = crate::kms::v2::KmsBackendV2::for_tests();

        install_backend_root_bindings(&mut state, &backend as &dyn Backend);

        let root = state.resources.window(ROOT_WINDOW).expect("root");
        assert_eq!(root.host_xid.map(|h| h.as_raw()), Some(backend.window_id()));
        let root_visual = state.resources.visual(ROOT_VISUAL).expect("root visual");
        assert_eq!(
            root_visual.host_visual_xid.map(|v| v.as_raw()),
            Some(backend.root_visual_xid())
        );
        let argb_visual = state.resources.visual(ARGB_VISUAL).expect("argb visual");
        assert_eq!(
            argb_visual.host_visual_xid.map(|v| v.as_raw()),
            backend.argb_visual_xid()
        );
        let argb_colormap = state
            .resources
            .colormap(ARGB_COLORMAP)
            .expect("argb colormap");
        assert_eq!(
            argb_colormap.host_colormap_xid.map(|c| c.as_raw()),
            backend.argb_colormap_xid()
        );
    }
}
