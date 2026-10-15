//! The server on the surface platform: headless (no surface, nothing shown,
//! only with an explicit `--headless`) or in a shell's window.

use std::{
    fs, io,
    path::PathBuf,
    sync::{Arc, atomic::AtomicUsize},
};

use yserver_core::{
    backend::{Backend as _, BackendTopology, install_backend_root_bindings},
    core_loop::{self, poll_tokens::ClientIdAllocator},
};

use super::{
    SurfacePlatform,
    clipboard::{ClipboardBridge, Pasteboard},
    clock::FrameClock,
    link::ShellLink,
    swapchain::{NativeSurface, SurfaceOutput},
    ui::ShellUi,
};
use crate::{
    kms::{
        render::XBackend,
        vk::device::{VkContext, VkContextOptions},
    },
    launch,
};

/// The screen size without `--geometry`, in X pixels.
pub(crate) const DEFAULT_SIZE: (u16, u16) = (1280, 800);

/// A window shell's half of the server: its surface, the surface's frame
/// clock (ticked by the shell), its link, its UI commands, the count of
/// established X clients the core keeps for it and the host pasteboard.
pub(crate) struct WindowSpec {
    pub(crate) surface: NativeSurface,
    pub(crate) clock: Arc<FrameClock>,
    pub(crate) link: Arc<ShellLink>,
    pub(crate) ui: Arc<dyn ShellUi>,
    pub(crate) clients: Arc<AtomicUsize>,
    /// The largest window content the screen shows unclamped, in X pixels.
    pub(crate) max_output: Option<(u16, u16)>,
    /// The window's physical size in millimetres; 0: unknown.
    pub(crate) output_mm: (u32, u32),
    /// Swapchain pixels per X pixel; later changes come as `Resized`.
    pub(crate) copy_scale: u32,
    /// Bridged with CLIPBOARD.
    pub(crate) pasteboard: Box<dyn Pasteboard>,
}

/// Run the headless server, or refuse without `--headless`.
///
/// # Errors
///
/// No `--headless`, or Vulkan, the sockets or the core loop failed.
pub(crate) fn run_headless(opts: launch::LaunchOptions) -> io::Result<()> {
    if !opts.headless {
        return Err(io::Error::other(
            "no window shell on this platform; use --headless for a server without a window",
        ));
    }
    let size = opts.geometry.unwrap_or(DEFAULT_SIZE);
    serve(
        &opts,
        |opts| {
            let vk = VkContext::new_with_options(VkContextOptions {
                wants_present: false,
                surface_extension: None,
                wants_kms_interop: false,
            })
            .map_err(|e| io::Error::other(format!("vulkan: {e}")))?;
            super::headless_backend(vk, size, opts.layout.clone())
        },
        None,
        None,
        |_| {},
    )
}

/// Run the server in the shell's window; tells the shell to quit when the
/// core loop ends. Called on the core thread.
///
/// # Errors
///
/// Vulkan, the sockets or the core loop failed.
pub(crate) fn run_windowed(opts: launch::LaunchOptions, spec: WindowSpec) -> io::Result<()> {
    let size = opts.geometry.unwrap_or(DEFAULT_SIZE);
    let WindowSpec {
        surface,
        clock,
        link,
        ui,
        clients,
        max_output,
        output_mm,
        copy_scale,
        pasteboard,
    } = spec;
    let result = serve(
        &opts,
        |opts| {
            let vk = surface.create_context()?;
            let extent = ash::vk::Extent2D {
                width: u32::from(size.0),
                height: u32::from(size.1),
            };
            let mut output = SurfaceOutput::new(Arc::clone(&vk), &surface, extent, clock)
                .map_err(|e| io::Error::other(format!("surface: {e:?}")))?;
            output.set_copy_scale(copy_scale);
            let mut backend = super::surface_backend(vk, size, opts.layout.clone(), vec![output])?;
            backend.pstate.link = Some(Arc::clone(&link));
            backend.pstate.ui = Some(Arc::clone(&ui));
            backend.pstate.client_count = clients;
            if let Some(max_output) = max_output {
                backend.platform.set_max_output(max_output);
            }
            backend.platform.output_mm = output_mm;
            Ok(backend)
        },
        Some(&link),
        Some(pasteboard),
        |display| ui.set_title(&format!("yserver DISPLAY=:{display}")),
    );
    link.disconnect();
    if let Err(e) = &result {
        log::error!("yserver: {e}");
    }
    ui.quit(result.is_err());
    result
}

/// Bring up the backend, the sockets and the core loop, and tear them down.
fn serve(
    opts: &launch::LaunchOptions,
    build: impl FnOnce(&launch::LaunchOptions) -> io::Result<XBackend<SurfacePlatform>>,
    link: Option<&ShellLink>,
    pasteboard: Option<Box<dyn Pasteboard>>,
    on_display: impl FnOnce(u16),
) -> io::Result<()> {
    log::info!(target: "yserver::startup", "yserver: startup — {}", crate::version::line());

    let auth = crate::build_auth_state(opts);
    crate::validate_tcp_startup(opts, &auth)?;
    let sigusr1_was_ignored = launch::sigusr1_is_ignored();
    let parent_pid = launch::startup_parent_pid();
    // Linux masks per thread, so this must precede every other thread; the
    // macOS kqueue variant ignores the signals process-wide.
    let signal_source = crate::block_termination_signals()?;

    let mut backend = build(opts)?;
    let topology = BackendTopology::from_backend(&mut backend);
    log::info!(
        "yserver: surface screen {}x{}",
        topology.width,
        topology.height
    );
    let mut state = topology.into_server_state();
    crate::clock::init(state.start_instant);
    install_backend_root_bindings(&mut state, &backend);

    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("create_dir_all({}): {e}", socket_dir.display()),
        )
    })?;
    let (display, listener, _lock_guard, socket_path) = match launch::resolve(opts) {
        launch::Resolution::Explicit { display, lock } => {
            let guard = if lock {
                Some(launch::acquire_lock(&PathBuf::from("/tmp"), display)?)
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
        launch::Resolution::FirstFree { first } => {
            let (display, guard, listener, socket_path) =
                launch::autopick_locked(&PathBuf::from("/tmp"), &socket_dir, first)?;
            (display, listener, Some(guard), socket_path)
        }
    };
    log::info!("yserver: listening on unix socket DISPLAY=:{display}");
    on_display(display);
    let listeners = crate::bind_client_listeners(listener, display, opts, &auth)?;
    let xdmcp = crate::build_xdmcp_service(opts, display)?;

    let (poll, sender, rx) = core_loop::channel()?;
    backend.set_input_sender(sender.clone_handle());
    crate::spawn_signal_thread(signal_source, sender.clone_handle(), opts.reset_policy)?;
    if let Some(link) = link {
        link.connect(sender.clone_handle());
    }
    launch::signal_ready(opts, display, sigusr1_was_ignored, parent_pid);
    // After the listener, stopped first: an internal client, never counted.
    let clipboard = pasteboard.and_then(|pasteboard| {
        ClipboardBridge::start(sender.clone_handle(), pasteboard)
            .inspect_err(|e| log::warn!("yserver: no clipboard bridge: {e}"))
            .ok()
    });

    log::info!("yserver: entering single-threaded core loop");
    let result = core_loop::run_core(
        poll,
        rx,
        sender,
        &mut state,
        &mut backend,
        listeners,
        &ClientIdAllocator::new(),
        auth,
        opts.reset_policy,
        xdmcp,
    );
    if let Some(clipboard) = clipboard {
        clipboard.stop();
    }
    if let Err(err) = &result {
        log::warn!("yserver: run_core returned error: {err}");
    }

    for entry in backend.take_shutdown_present_events() {
        yserver_core::core_loop::process_request::fire_present_completion_events(
            &mut state, &entry,
        );
    }
    backend.signal_all_retained_present_wakes();
    yserver_core::core_loop::process_request::shutdown_drain_present_pending_exec(
        &mut state,
        &mut backend,
    );
    backend.shutdown_destroy_drawables();
    let _ = fs::remove_file(&socket_path);
    log::info!("yserver: exiting");
    result
}
