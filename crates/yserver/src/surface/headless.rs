//! The server on the surface platform with no surface: X clients connect
//! and draw, nothing is shown. Started only with an explicit `--headless`
//! until a window shell creates the first surface.

use std::{fs, io, path::PathBuf};

use yserver_core::{
    backend::{Backend as _, BackendTopology, install_backend_root_bindings},
    core_loop::{self, poll_tokens::ClientIdAllocator},
};

use crate::{
    kms::vk::device::{VkContext, VkContextOptions},
    launch,
};

/// The logical screen size of the headless server.
const HEADLESS_SIZE: (u16, u16) = (1600, 1000);

/// Run the headless server, or refuse without `--headless`.
///
/// # Errors
///
/// No `--headless`, or Vulkan, the sockets or the core loop failed.
pub(crate) fn run(opts: launch::LaunchOptions) -> io::Result<()> {
    if !opts.headless {
        return Err(io::Error::other(
            "no window backend yet (M1); use --headless for a server without a window",
        ));
    }
    log::info!(target: "yserver::startup", "yserver: startup — {}", crate::version::line());

    let auth = crate::build_auth_state(&opts);
    crate::validate_tcp_startup(&opts, &auth)?;
    let sigusr1_was_ignored = launch::sigusr1_is_ignored();
    let parent_pid = launch::startup_parent_pid();
    // Before any thread exists, so every thread inherits the signal mask.
    let signal_source = crate::block_termination_signals()?;

    let vk = VkContext::new_with_options(VkContextOptions {
        wants_present: false,
        surface_extension: None,
        wants_kms_interop: false,
    })
    .map_err(|e| io::Error::other(format!("vulkan: {e}")))?;
    let mut backend = super::headless_backend(vk, HEADLESS_SIZE, opts.layout.clone())?;
    let topology = BackendTopology::from_backend(&mut backend);
    log::info!(
        "yserver: headless screen {}x{}",
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
    let (display, listener, _lock_guard, socket_path) = match launch::resolve(&opts) {
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
    };
    log::info!("yserver: listening on unix socket DISPLAY=:{display}");
    let listeners = crate::bind_client_listeners(listener, display, &opts, &auth)?;
    let xdmcp = crate::build_xdmcp_service(&opts, display)?;

    let (poll, sender, rx) = core_loop::channel()?;
    backend.set_input_sender(sender.clone_handle());
    crate::spawn_signal_thread(signal_source, sender.clone_handle(), opts.reset_policy)?;
    launch::signal_ready(&opts, display, sigusr1_was_ignored, parent_pid);

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
