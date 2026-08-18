pub mod clock;
pub mod drm;
pub mod input;
pub mod input_thread;
#[doc(hidden)]
pub mod internal_probe;
pub mod kms;
pub mod launch;
pub(crate) mod platform;
pub mod present;
pub mod version;
mod vt;

use std::{fs, io, path::PathBuf, thread};

use nix::sys::signal::{SigSet, Signal};
#[cfg(target_os = "linux")]
use nix::sys::signal::{SigmaskHow, sigprocmask};
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

/// Refuse to start when libinput's initial seat enumeration opened zero
/// **usable** (keyboard- or pointer-capable) input devices. A display server
/// with no keyboard or mouse is unusable — you can't even zap out — so we fail
/// fast with an actionable error instead of coming up dead (issue #64).
///
/// `opened` counts keyboard/pointer-capable devices only. This matters when the
/// process lacks input access (e.g. started over SSH, not from the console): the
/// real keyboard/mouse are permission-denied, yet a lone non-usable node (e.g. a
/// HID "System Control" collection) can still open — that must not satisfy the
/// guard.
fn ensure_input_devices_opened(opened: usize) -> io::Result<()> {
    if opened == 0 {
        return Err(io::Error::other(
            "no usable input devices (keyboard or pointer) could be opened under \
             /dev/input (the device nodes exist but are permission-denied).\n\
             yserver needs direct access to input devices: add the user to the \
             'input' group, and start it from the console — not over SSH.",
        ));
    }
    Ok(())
}

/// What the startup path should do about input: spawn the input thread if a
/// libinput context was created, or refuse to start if not.
#[derive(Debug, PartialEq, Eq)]
enum InputStartup {
    /// A live libinput context — spawn the input thread.
    DirectSpawn,
    /// No libinput context at all — refuse to start.
    AbortNoInput,
}

/// Decide the input-startup action. yserver is always Direct and requires
/// input: coming up without it yields a session that is dead on arrival and
/// cannot even be zapped (zap is itself an input event), so we refuse to start.
/// `has_input_ctx == false` means `SendContext::new()` failed — libinput was
/// unavailable or every input device was permission-denied (not in the `input`
/// group / not on the console). Extracted as a pure fn so the abort is
/// unit-testable; `run()` itself is not.
fn input_startup_action(has_input_ctx: bool) -> InputStartup {
    if has_input_ctx {
        InputStartup::DirectSpawn
    } else {
        InputStartup::AbortNoInput
    }
}

pub fn run(opts: launch::LaunchOptions) -> io::Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    panic!("yserver only supports Linux and FreeBSD (DRM/KMS, libinput, evdev)");

    log::info!("yserver: startup — {}", crate::version::line());

    // Capture the inherited SIGUSR1 disposition before signalfd masking.
    // If the DM started us with SIGUSR1 ignored, we signal it when ready.
    let sigusr1_was_ignored = launch::sigusr1_is_ignored();

    // Capture the parent (DM) PID now, before long init — if the parent
    // dies during startup and we get reparented, getppid() at readiness
    // would point at a subreaper or PID 1. Xorg captures it the same way.
    let parent_pid = launch::startup_parent_pid();

    // Block the termination signals and take the signalfd BEFORE spawning
    // ANY thread. A process-directed signal (e.g. the `kill -TERM` a
    // launcher sends at shutdown) is delivered by the kernel to an
    // ARBITRARY thread that has not blocked it. If even one thread has
    // SIGTERM unblocked — e.g. the `YSERVER_LOOP_TELEMETRY` vk-call-rate
    // thread spawned just below, which previously started before this
    // point and so inherited the empty startup mask — the signal lands
    // there and runs the DEFAULT action, terminating the process WITHOUT
    // the signalfd, the graceful `Message::Shutdown`, or
    // `ConsoleGuard::Drop`. On a VC that left the console dead (K_OFF +
    // KD_GRAPHICS never restored) — the telemetry-mode-logout hang. Block
    // here, first, so every later-spawned thread inherits the mask and the
    // signal can only reach the signalfd. MUST stay after
    // `sigusr1_is_ignored()` above, which reads the inherited SIGUSR1
    // disposition before we mask it.
    let signal_fd = block_termination_signals()?;

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
                // Render-batch flush attribution — why an open
                // PendingRenderBatch closed its render pass. Sizes the
                // same-target render-pass coalescing phases:
                // key_change_same_dst + the per-kind buckets are the
                // merge opportunity; key_change_diff_dst / readback /
                // present are genuine pass boundaries; self_sample
                // bounds the realistic win (these must flush even under
                // a generalised same-dst session). `other` should stay
                // near-zero.
                log::info!(
                    "vk renderpass flush src [1s]: key_change_same_dst={} \
                     key_change_diff_dst={} fill={} copy={} glyph={} traps={} \
                     put_image={} readback={} present={} other={} self_sample={}",
                    s.rpflush_key_change_same_dst,
                    s.rpflush_key_change_diff_dst,
                    s.rpflush_for_fill,
                    s.rpflush_for_copy,
                    s.rpflush_for_glyph,
                    s.rpflush_for_traps,
                    s.rpflush_for_put_image,
                    s.rpflush_for_readback,
                    s.rpflush_for_present,
                    s.rpflush_for_other,
                    s.rp_self_sample,
                );
                // Frame-builder close-replay coalescing — the ACTUAL
                // render-pass hot path for compositing desktops (xfce).
                // pass_ops ≈ begin_rendering/s; coalescable = passes a
                // same-dst session could merge away (all-slices ceiling),
                // partitioned into: mergeable (Slice 1, composite-only,
                // fold-clean) + dirty_clear (Slice 1.5, blocked only by a
                // solid clear → per-op scratch unlocks) + cross_kind
                // (Slice 2, needs the recorder split). self_sample bounds
                // the headroom (composites reading their own dst).
                log::info!(
                    "vk frame coalescing [1s]: pass_ops={} coalescable={} mergeable={} dirty_clear={} cross_kind={} self_sample={}",
                    s.fb_pass_ops,
                    s.fb_pass_coalescable,
                    s.fb_pass_mergeable,
                    s.fb_coalescable_dirty_clear,
                    s.fb_coalescable_cross_kind,
                    s.fb_self_sample,
                );
                // PixmapPool deltas — cumulative counters minus the
                // previous snapshot. Tells us per second whether the
                // pool is being consulted (takes_hit+takes_miss),
                // whether mirrors return to it (returns_accepted),
                // and which rejection path fires (bucket_full means
                // PIXMAP_POOL_BUCKET_BUDGET_BYTES is too small for
                // this key's working set; oversize means
                // MAX_POOLED_DIM is too small).
                //
                // bucket_full tracking takes_miss 1:1 is the signature
                // of a cap-bound pool: every rejected return becomes a
                // kernel allocation on the next take.
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

    // Take over the console TTY before opening anything else: stops the
    // kernel keyboard driver from delivering Ctrl-C / Ctrl-Z / etc. as
    // signals to the controlling TTY's foreground process group, which
    // would otherwise kill the whole session when the user hits Ctrl-C
    // inside an X client. Skipped silently when not on a Linux VC (pty
    // under SSH or a graphical terminal emulator).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let console_guard = crate::kms::console::ConsoleGuard::acquire(opts.vt)?;
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let console_guard: Option<()> = None;
    let device_paths = crate::platform::drm::resolve_default_kms_devices()?;
    if device_paths.is_empty() {
        log::info!("yserver: no DRM devices to open; starting zero-card headless");
    } else {
        log::info!(
            "yserver: opening DRM devices (primary first): {}",
            device_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Always Direct (self-managed DRM master + VT_PROCESS): open DRM +
    // libinput directly, arming VT_PROCESS when a controlling console is
    // present.
    let mut backend = build_kms_backend(&device_paths, console_guard, opts.layout.clone())?;
    let (fb_w, fb_h) = backend.fb_dimensions();
    log::info!("yserver: scanout {fb_w}x{fb_h}");

    let (randr_outputs, randr_mode_table) = backend.randr_outputs_and_modes();
    let randr_providers = backend.randr_providers();
    let capabilities = yserver_core::server::BackendCapabilities::from_backend(&backend);
    let mut state = ServerState::with_randr_outputs_and_modes(
        fb_w,
        fb_h,
        randr_outputs,
        randr_mode_table,
        capabilities,
    );
    state.randr.set_providers(randr_providers);
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
    // Backend-originated failures (GPU/device loss, unrecoverable VT resume
    // errors) use the same core-channel shutdown path as input hotkeys.
    backend.set_input_sender(sender.clone_handle());

    // Spawn the dedicated libinput sender thread. After `take_input_ctx`
    // the backend's `poll_fds()` no longer returns the libinput fd, so
    // run_core's E3 registration step won't double-poll libinput.
    // Take the libinput context (created in `platform_init`). `None` means
    // `SendContext::new()` failed — libinput unavailable, or every input
    // device was permission-denied — so we refuse to start below.
    let direct_input_ctx = backend.take_input_ctx();
    match input_startup_action(direct_input_ctx.is_some()) {
        InputStartup::AbortNoInput => {
            // libinput could not be set up at all (`SendContext::new()` failed
            // → `take_input_ctx()` is None). Refuse to start: a session with no
            // input is dead on arrival and cannot even be zapped.
            ensure_input_devices_opened(0)?;
            unreachable!("ensure_input_devices_opened(0) always returns Err");
        }
        InputStartup::DirectSpawn => {
            let mut input_ctx =
                direct_input_ctx.expect("DirectSpawn action implies Some(input_ctx)");
            // Drain libinput's initial seat enumeration here, on the main thread,
            // BEFORE signalling readiness — so we can refuse to start when no input
            // device could be opened (issue #64) rather than coming up with a dead
            // keyboard and mouse. `udev_assign_seat` queues `DeviceAdded`
            // synchronously and a single dispatch consumes the initial enumeration
            // (the same contract the input thread relied on). The drained events
            // are handed to the thread so device registration is unchanged.
            let initial_events = match input_ctx.dispatch() {
                Ok(evs) => evs,
                Err(err) => {
                    log::warn!("yserver: initial libinput dispatch: {err}");
                    Vec::new()
                }
            };
            // Count only USABLE (keyboard- or pointer-capable) devices, not
            // every DeviceAdded: when not on seat0, the real keyboard/mouse
            // are permission-denied while a lone non-usable node (e.g. a HID
            // "System Control" collection) may still open — that must NOT
            // satisfy the guard, or we come up with a dead, un-zappable
            // session. The context tracks capability at add time.
            let opened = input_ctx.usable_input_device_count();
            ensure_input_devices_opened(opened)?;
            log::info!("yserver: {opened} usable input device(s) opened at startup");

            let input_sender = sender.clone_handle();
            let input_control =
                std::sync::Arc::new(crate::input_thread::InputThreadControl::new()?);
            // Lock-LED relay: the core thread owns the XKB lock state, the
            // input thread owns the libinput devices; LED transitions
            // cross via this eventfd-backed mask.
            let led_relay = std::sync::Arc::new(crate::input::LedRelay::new()?);
            backend.set_input_thread_control(std::sync::Arc::clone(&input_control));
            backend.set_led_relay(std::sync::Arc::clone(&led_relay));
            log::info!("yserver: Direct mode — spawning libinput sender thread");
            // Seed the input thread's cursor at the primary-output centre so it
            // agrees with the core's startup position (Xorg-style warp to display 0).
            let (init_cx, init_cy) = backend.initial_pointer_position();
            thread::Builder::new()
                .name("yserver-libinput".into())
                .spawn(move || {
                    if let Err(err) = input_thread::run(
                        input_ctx,
                        initial_events,
                        input_sender,
                        u32::from(fb_w),
                        u32::from(fb_h),
                        init_cx,
                        init_cy,
                        input_control,
                        led_relay,
                    ) {
                        log::warn!("yserver: libinput thread exited: {err}");
                    }
                })?;
        }
    }

    // signalfd → Message bridge. yserver-core deliberately doesn't
    // depend on nix; a tiny thread wraps the SignalFd read so run_core
    // only sees channel-side messages. SIGINT/SIGTERM map to
    // `Shutdown`; SIGUSR1/SIGUSR2 map to VT release/acquire messages
    // (a no-op when VT switching is not armed).
    //
    // SIGUSR1 carries three distinct, non-conflicting meanings here:
    // (1) the *inherited disposition* read once at startup
    // (`sigusr1_was_ignored` above) drives the readiness handshake
    // *to the parent* DM (`launch::signal_ready`); (2) masked-and-
    // signalfd-consumed *delivery to self* drives the VT release/
    // acquire handshake; (3) we *send* SIGUSR1 outward to the parent
    // at readiness. Disposition-in, delivery-to-self, and signal-out
    // are separate.
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
    let auth = core_loop::auth::AuthState::new(opts.auth_file.clone());
    if opts.auth_file.is_some() {
        log::info!(
            "yserver: authorization enabled via -auth {:?}",
            opts.auth_file
        );
    } else {
        log::info!("yserver: no -auth file; local access open (Xorg default)");
    }
    log::info!("yserver: entering single-threaded core loop");
    let result = core_loop::run_core(
        poll,
        rx,
        sender,
        &mut state,
        &mut backend,
        Some(listener),
        &alloc,
        auth,
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

    // `fire_pending_present_entry` now retains wake pins instead of signalling
    // them (core paces at vblank). At teardown there is no more vblank clock,
    // so flush every still-retained wake to release the buffers clients are
    // blocked on. This replaces the release the old shutdown/force path did.
    backend.signal_all_retained_present_wakes();
    // Symmetric flush for the *pre*-copy half: any Present still parked in
    // the unified pending-present store (source wait not yet signalled)
    // would otherwise leak its entry pin and strand the client's idle
    // fence / release syncobj forever once the socket goes away.
    yserver_core::core_loop::process_request::shutdown_drain_present_pending_exec(
        &mut state,
        &mut backend,
    );

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

/// Build a `KmsBackend`. yserver is always Direct — self-managed DRM master
/// with VT_PROCESS — so this opens DRM + libinput directly, arming VT_PROCESS
/// when a real controlling console is present. (libseat/logind session
/// management was removed; the Direct model is the sole seat model.)
fn build_kms_backend(
    device_paths: &[std::path::PathBuf],
    console_guard: crate::kms::ConsoleGuardOpt,
    layout: Option<String>,
) -> io::Result<crate::kms::render::KmsBackend> {
    crate::kms::render::KmsBackend::open(device_paths, console_guard, layout)
}

#[cfg(target_os = "linux")]
fn block_termination_signals() -> io::Result<SignalFd> {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    // SIGHUP → route through the signalfd → graceful shutdown, same as
    // SIGTERM. On session/logout the kernel can HUP the process; with
    // SIGHUP at its default disposition the process terminates WITHOUT
    // running `Drop`, so `ConsoleGuard` never restores the VT and the
    // console is left dead (K_OFF + KD_GRAPHICS). Blocking it here makes
    // the signalfd log it ("received signal 1") and drive the graceful
    // path that restores the console. (dirty-exit-on-telemetry-logout hunt)
    mask.add(Signal::SIGHUP);
    // SIGUSR1 → VT release (see the signalfd loop). Blocked so signalfd
    // consumes it instead of the default action, which would terminate us.
    mask.add(Signal::SIGUSR1);
    // SIGUSR2 → VT acquire. Same blocking rationale as SIGUSR1.
    mask.add(Signal::SIGUSR2);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| io::Error::other(format!("sigprocmask SIG_BLOCK: {err}")))?;
    SignalFd::new(&mask).map_err(|err| io::Error::other(format!("signalfd: {err}")))
}

/// FreeBSD: ignore the same signals and return a kqueue fd with
/// EVFILT_SIGNAL filters registered.
#[cfg(target_os = "freebsd")]
fn block_termination_signals() -> io::Result<nix::sys::event::Kqueue> {
    use nix::sys::{
        event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue},
        signal::{SaFlags, SigAction, SigHandler, sigaction},
    };

    // Unlike signalfd, EVFILT_SIGNAL is reported after normal signal
    // delivery processing. Blocking these signals keeps them pending and
    // prevents the kqueue event from arriving. Ignoring them avoids default
    // termination while still letting kqueue record each delivery attempt.
    let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    for sig in [
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGHUP,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
    ] {
        // SAFETY: installing SIG_IGN for process-control signals before
        // worker threads are spawned; kqueue is the synchronous consumer.
        unsafe { sigaction(sig, &ignore) }
            .map_err(|err| io::Error::other(format!("sigaction {sig:?} SIG_IGN: {err}")))?;
    }

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
            libc::SIGHUP as usize,
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
    use super::{
        InputStartup, ensure_input_devices_opened, input_startup_action,
        install_backend_root_bindings,
    };
    use yserver_core::{
        backend::Backend,
        resources::{ARGB_COLORMAP, ARGB_VISUAL, ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };

    #[test]
    fn install_backend_root_bindings_sets_root_host_xid_and_visuals() {
        let mut state = ServerState::new();
        let backend = crate::kms::render::KmsBackend::for_tests();

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

    #[test]
    fn ensure_input_devices_opened_rejects_zero_with_actionable_message() {
        let err = ensure_input_devices_opened(0).expect_err("zero devices must abort startup");
        let msg = err.to_string();
        // The message must steer the user to the real cause, not just fail.
        assert!(msg.contains("/dev/input"), "missing device path: {msg}");
        assert!(
            msg.contains("'input' group"),
            "missing input-group hint: {msg}"
        );
        assert!(
            msg.contains("console") || msg.contains("SSH"),
            "missing console/SSH hint: {msg}"
        );
    }

    #[test]
    fn ensure_input_devices_opened_accepts_one_or_more() {
        assert!(ensure_input_devices_opened(1).is_ok());
        assert!(ensure_input_devices_opened(8).is_ok());
    }

    #[test]
    fn direct_mode_without_input_ctx_aborts_startup() {
        // Regression: always-Direct with a failed libinput setup
        // (`SendContext::new()` Err → `take_input_ctx()` None) must abort,
        // not fall through to a dead session that can't be zapped. This is
        // the gap the `opened == 0` guard did NOT cover.
        assert_eq!(input_startup_action(false), InputStartup::AbortNoInput);
    }

    #[test]
    fn with_input_ctx_spawns_input_thread() {
        assert_eq!(input_startup_action(true), InputStartup::DirectSpawn);
    }
}
