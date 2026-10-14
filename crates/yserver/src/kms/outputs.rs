//! KMS output bring-up: output identity, the active-output inventory and the
//! initial modeset that `PlatformBackend::open_with_commit` delegates into.

use std::{
    io,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::{drm, kms::scanout_route::ScanoutRoute};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct OutputKey {
    pub(crate) device_key: crate::platform::drm::DrmDeviceKey,
    pub(crate) connector_name: String,
}

impl OutputKey {
    pub(crate) fn new(
        device_key: crate::platform::drm::DrmDeviceKey,
        connector_name: impl Into<String>,
    ) -> Self {
        Self {
            device_key,
            connector_name: connector_name.into(),
        }
    }
}

/// A single DRM output and its dedicated swapchain, positioned in the
/// virtual screen. The stable key keeps equal connector names on different
/// devices distinct. The render `PlatformBackend` owns one of these per
/// discovered output; `fb_w` / `fb_h` describe the virtual-screen
/// extent.
pub(crate) struct ActiveOutput {
    pub key: OutputKey,
    /// Renderer and KMS endpoints that own this live output's scanout route.
    /// This is qualified only after Vulkan selects its operational renderer;
    /// init-time dumb scanout uses [`PlatformInitOutput`] instead.
    pub scanout_route: ScanoutRoute,
    pub output: crate::platform::drm::Output,
    /// Kept alive for the lifetime of the output to retain initial-
    /// scanout buffer ownership; v2 has its own per-output
    /// `ScanoutBoPool` and doesn't read this field after construction.
    #[allow(dead_code)]
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl ActiveOutput {
    pub(crate) fn new(
        scanout_route: ScanoutRoute,
        output: crate::platform::drm::Output,
        swapchain: crate::drm::Swapchain,
        x: i32,
        y: i32,
    ) -> Self {
        let key = OutputKey::new(scanout_route.kms_device_key, output.connector_name.clone());
        let width = output.picked.width;
        let height = output.picked.height;
        Self {
            key,
            scanout_route,
            output,
            swapchain,
            x,
            y,
            width,
            height,
        }
    }
}

/// Output committed during platform discovery, before Vulkan has selected a
/// renderer endpoint. It is converted into [`ActiveOutput`] only after a
/// truthful [`ScanoutRoute`] can be constructed.
pub(crate) struct PlatformInitOutput {
    pub key: OutputKey,
    pub output: crate::platform::drm::Output,
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl PlatformInitOutput {
    fn new(
        device_key: crate::platform::drm::DrmDeviceKey,
        output: crate::platform::drm::Output,
        swapchain: crate::drm::Swapchain,
        x: i32,
        y: i32,
    ) -> Self {
        let key = OutputKey::new(device_key, output.connector_name.clone());
        let width = output.picked.width;
        let height = output.picked.height;
        Self {
            key,
            output,
            swapchain,
            x,
            y,
            width,
            height,
        }
    }

    /// Attach the post-Vulkan renderer identity without reopening, cloning, or
    /// otherwise disturbing the already-committed dumb scanout resources.
    pub(crate) fn qualify(self, scanout_route: ScanoutRoute) -> ActiveOutput {
        debug_assert_eq!(self.key.device_key, scanout_route.kms_device_key);
        ActiveOutput {
            key: self.key,
            scanout_route,
            output: self.output,
            swapchain: self.swapchain,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// Centre of the primary output (output 0) — the startup pointer position,
/// matching Xorg which warps the pointer to the centre of the first display.
/// On a multi-head layout the framebuffer centre lands on the seam between
/// monitors, so we centre on output 0 instead. Falls back to the framebuffer
/// centre when no outputs are known.
pub(crate) fn primary_output_center(outputs: &[ActiveOutput], fb_w: u16, fb_h: u16) -> (i32, i32) {
    outputs.first().map_or_else(
        || (i32::from(fb_w) / 2, i32::from(fb_h) / 2),
        |o| (o.x + i32::from(o.width) / 2, o.y + i32::from(o.height) / 2),
    )
}

/// One DRM/KMS device opened during platform bring-up.
pub(crate) struct PlatformInitDevice {
    pub(crate) key: crate::platform::drm::DrmDeviceKey,
    pub(crate) device: Rc<drm::Device>,
}

/// Transient handoff from device discovery to the long-lived renderer.
///
/// The ordered device vector carries every successfully opened KMS card. Only
/// the first entry receives startup scanout in this incremental PRIME step;
/// secondary entries exist for provider identity and later device routing.
/// Empty `devices` and `layouts` is a valid zero-card headless platform.
pub(crate) struct PlatformInit {
    pub(crate) devices: Vec<PlatformInitDevice>,
    /// Selected render endpoint, separate from the KMS-device inventory.
    /// `None` is valid for headless startup or unavailable DRI3.
    pub(crate) render_node: Option<crate::kms::render_node::OpenedRenderNode>,
    pub(crate) layouts: Vec<PlatformInitOutput>,
    pub(crate) fb_w: u16,
    pub(crate) fb_h: u16,
    pub(crate) input_ctx: Option<crate::input::SendContext>,
}

fn render_node_for_device(
    device_path: &str,
    device: &drm::Device,
) -> Option<crate::kms::render_node::OpenedRenderNode> {
    match crate::kms::render_node::open_for_card(device) {
        Ok(render_node) => {
            log::info!(
                "DRI3 render node ready for {device_path}: fd={} path={:?} rdev={} \
                 (render node minor should be >=128)",
                render_node.raw_fd(),
                render_node.path(),
                render_node.key(),
            );
            Some(render_node)
        }
        Err(err) => {
            log::warn!(
                "DRI3 render node unavailable for selected display {device_path}: {err}; \
                 renderer selection may continue only through the explicit unverified fallback \
                 when no suitable Vulkan device exposes VK_EXT_physical_device_drm"
            );
            None
        }
    }
}

fn activate_initial_scanout_outputs(
    device_key: crate::platform::drm::DrmDeviceKey,
    device: &Rc<drm::Device>,
    commit: fn(
        &crate::drm::Device,
        &crate::platform::drm::Output,
        ::drm::control::framebuffer::Handle,
    ) -> io::Result<()>,
) -> io::Result<Vec<PlatformInitOutput>> {
    let outputs = crate::platform::drm::discover_outputs(device)?;
    if !outputs.is_empty() {
        // Refuse software-only Vulkan before allocating or committing any
        // scanout buffer. An opened card with no connected outputs remains a
        // valid headless platform; later RANDR enable repeats this guard.
        crate::kms::vk::device::ensure_hardware_vulkan_for_scanout().map_err(io::Error::other)?;
    }

    // Horizontal layout in connector order. If anything fails part way
    // through bring-up, disable everything already committed so the caller
    // starts from a clean slate.
    let mut layouts = Vec::with_capacity(outputs.len());
    let mut next_x = 0_i32;
    let mut bring_up_err = None;
    for output in outputs {
        let w = output.picked.width;
        let h = output.picked.height;
        let mut buffers = Vec::with_capacity(2);
        let mut buffer_err = None;
        for _ in 0..2 {
            match drm::Buffer::new(Rc::clone(device), w, h) {
                Ok(buffer) => buffers.push(buffer),
                Err(err) => {
                    buffer_err = Some(err);
                    break;
                }
            }
        }
        if let Some(err) = buffer_err {
            bring_up_err = Some(err);
            break;
        }
        let initial_fb = buffers[0].fb_id();
        if let Err(err) = commit(device, &output, initial_fb) {
            bring_up_err = Some(err);
            break;
        }
        let swapchain = drm::Swapchain::with_initial_scanout(buffers, 0);
        layouts.push(PlatformInitOutput::new(
            device_key, output, swapchain, next_x, 0,
        ));
        next_x = next_x.saturating_add(i32::from(w));
    }
    if let Some(err) = bring_up_err {
        for done in layouts.iter_mut().rev() {
            if let Err(disable_err) = drm::modeset::disable_output(device, &done.output) {
                log::warn!(
                    "initial scanout rollback: failed to disable {}: {disable_err}; \
                     leaving its buffers for DRM-fd close",
                    done.output.connector_name,
                );
                done.swapchain.disarm();
            }
        }
        return Err(err);
    }
    Ok(layouts)
}

/// Reject two paths that resolve to the same kernel DRM device identity.
fn validate_unique_kms_device_identity(
    opened: &[(crate::platform::drm::DrmDeviceKey, PathBuf)],
    key: crate::platform::drm::DrmDeviceKey,
    path: &Path,
) -> io::Result<()> {
    let Some((_, existing_path)) = opened.iter().find(|(existing, _)| *existing == key) else {
        return Ok(());
    };
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "DRM device {} aliases already-opened device {} (same rdev {key})",
            path.display(),
            existing_path.display(),
        ),
    ))
}

/// Shared DRM / outputs / libinput bring-up for the v1 and v2
/// backends. Extracted in Stage 1b so both `KmsBackend::open_with_commit`
/// and `KmsBackend::open` use the same code path.
///
/// **Vulkan / pipelines / scanout pools / scheduler / pixmap pool**
/// stay in the v1-specific portion of `open_with_commit` for now —
/// v2 doesn't build any of that in Stage 1b (paint paths are
/// stubbed). Stage 2 promotes the appropriate subset into the
/// real `PlatformBackend` component.
///
/// # Errors
///
/// Individual DRM-open failures are logged and skipped. Once a device opens,
/// identity/discovery/commit failures remain fatal. If no device opens, the
/// returned platform is headless. On bring-up error any output already
/// committed gets disabled before returning so the next caller starts clean.
pub(crate) fn platform_init(
    device_paths: &[PathBuf],
    commit: fn(
        &crate::drm::Device,
        &crate::platform::drm::Output,
        ::drm::control::framebuffer::Handle,
    ) -> io::Result<()>,
) -> io::Result<PlatformInit> {
    let mut devices = Vec::with_capacity(device_paths.len());
    let mut opened_device_paths = Vec::with_capacity(device_paths.len());
    let mut render_node = None;
    let mut layouts = Vec::new();
    let mut open_errors = Vec::new();
    for device_path in device_paths {
        let device_path_str = device_path.to_string_lossy().into_owned();
        let device = match drm::Device::open(&device_path_str) {
            Ok(device) => Rc::new(device),
            Err(err) => {
                log::warn!(
                    "yserver: skipping DRM device {}: open failed: {err}",
                    device_path.display()
                );
                open_errors.push(format!("{}: open failed: {err}", device_path.display()));
                continue;
            }
        };
        let device_key =
            crate::platform::drm::primary_device_key_from_fd(std::os::fd::AsFd::as_fd(&*device))?;
        validate_unique_kms_device_identity(&opened_device_paths, device_key, device_path)?;
        opened_device_paths.push((device_key, device_path.clone()));

        if devices.is_empty() {
            render_node = render_node_for_device(&device_path_str, &device);
        }
        if !devices.is_empty() {
            log::info!(
                "yserver: opened secondary KMS device {} as provider/topology data; \
                 initial scanout remains on the first opened device",
                device_path.display()
            );
        }
        devices.push(PlatformInitDevice {
            key: device_key,
            device,
        });
    }

    // Finish every fallible per-device identity qualification before the
    // first modeset. Otherwise a bad secondary card could abort startup after
    // the primary was already scanning out, bypassing the renderer's
    // construction-wide rollback guard.
    if let Some(primary) = devices.first() {
        layouts = activate_initial_scanout_outputs(primary.key, &primary.device, commit)?;
    }

    if devices.is_empty() {
        if open_errors.is_empty() {
            log::info!("platform_init: no DRM devices supplied; starting headless");
        } else {
            log::warn!(
                "platform_init: no DRM devices opened; starting headless. Tried:\n  {}",
                open_errors.join("\n  ")
            );
        }
    }

    // fb_w / fb_h carry the virtual-screen extent. Saturating
    // cast: huge layouts that exceed u16 are clamped — the rest
    // of the backend assumes u16 framebuffer dims.
    let fb_w: u16 = layouts
        .iter()
        .map(|l| u16::try_from(l.x.saturating_add(i32::from(l.width))).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    let fb_h: u16 = layouts
        .iter()
        .map(|l| u16::try_from(l.y.saturating_add(i32::from(l.height))).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);

    let input_ctx = match crate::input::SendContext::new() {
        Ok(ctx) => Some(ctx),
        Err(err) => {
            // Note: not a decision point. `run()` treats a missing context as
            // fatal and refuses to start (see `input_startup_action`).
            log::warn!("libinput SendContext unavailable: {err}");
            None
        }
    };

    Ok(PlatformInit {
        devices,
        render_node,
        layouts,
        fb_w,
        fb_h,
        input_ctx,
    })
}

#[cfg(test)]
mod platform_init_tests {
    use super::*;

    fn unused_commit(
        _device: &crate::drm::Device,
        _output: &crate::platform::drm::Output,
        _fb: ::drm::control::framebuffer::Handle,
    ) -> io::Result<()> {
        unreachable!("a zero-device platform must not commit an output")
    }

    #[test]
    fn duplicate_kms_identity_rejects_distinct_alias_paths() {
        let key = crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 1,
        };
        let opened = [(key, PathBuf::from("/dev/dri/card1"))];

        validate_unique_kms_device_identity(
            &opened,
            crate::platform::drm::DrmDeviceKey {
                major: 226,
                minor: 0,
            },
            Path::new("/dev/dri/card0"),
        )
        .expect("a distinct DRM identity is allowed");

        let error = validate_unique_kms_device_identity(
            &opened,
            key,
            Path::new("/dev/dri/by-path/pci-0000:01:00.0-card"),
        )
        .expect_err("two paths naming the same DRM identity must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let message = error.to_string();
        assert!(message.contains("/dev/dri/card1"));
        assert!(message.contains("/dev/dri/by-path/pci-0000:01:00.0-card"));
        assert!(message.contains("226:1"));
    }

    #[test]
    fn accepts_empty_device_list_as_headless() {
        let init = platform_init(&[], unused_commit)
            .expect("an empty DRM device list should produce a headless platform");
        assert!(init.devices.is_empty());
        assert!(init.layouts.is_empty());
        assert_eq!((init.fb_w, init.fb_h), (0, 0));
    }

    #[test]
    fn accepts_all_open_failures_as_headless() {
        let suffix = std::process::id();
        let paths = [
            std::env::temp_dir().join(format!("yserver-missing-drm-{suffix}-a")),
            std::env::temp_dir().join(format!("yserver-missing-drm-{suffix}-b")),
        ];
        let init = platform_init(&paths, unused_commit)
            .expect("missing DRM paths should produce a headless platform");
        assert!(init.devices.is_empty());
        assert!(init.layouts.is_empty());
        assert_eq!((init.fb_w, init.fb_h), (0, 0));
    }
}
