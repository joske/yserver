mod crossings;
mod cursor;
mod deadlines;
mod dri3;
mod gc_draw;
mod keyboard_source;
mod misc;
mod paint_target;
mod pointer;
mod present_scanout;
mod randr;
mod render_clip;
mod render_picture;
mod root_scanout;
mod stacking;
mod vt_session;
mod window_border;
mod window_tree;
mod xi_device_enabled;
mod xi_grabs_classes;
mod xi_hotplug_session;
mod xi_xtest;
mod xkb;

use super::{
    CrtcConfigProbeCompletion, CrtcConfigProbeExecutor, CrtcConfigProbeJob, KmsBackend,
    PaintTarget, PictureRecord, RandrIdAllocator, RandrProviderEndpoint,
    composite_needs_source_snapshot, compute_copy_area_dst_rects, compute_render_composite_clip,
    dri3_import_supported_for_topology, dri3_version_for, dst_picture_clip_by_children,
    glx_vendor_names_for_driver, intersect_rect_with_clip, mode_timing, reconcile_connector_probe,
    restore_primary_output_after_rebuild, shared_backing_move_pieces,
};
use crate::{
    internal_probe::{ProbeKmsHandles, RouteProbeRequest},
    kms::{
        backend::OutputKey,
        cpu_types::{Rectangle16, Repeat},
        render::{
            platform::{
                ConnectorSnapshot, CrtcKey, PlatformBackend, QualifiedScanoutPlan, RenderDevice,
                RenderDeviceId,
            },
            store::Storage,
        },
        scanout_route::{RenderKmsRelationship, ScanoutRoute},
        vk::scanout::ScanoutAllocationPlan,
    },
    platform::drm::DrmDeviceKey,
};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io,
    rc::Rc,
};
use yserver_core::{
    backend::{Backend, CrtcConfigApply, CrtcConfigToken, Dri3Caps, ModeSpec},
    core_loop::{CoreSender, Message},
    server::ServerState,
};

fn test_device_key(minor: u32) -> DrmDeviceKey {
    DrmDeviceKey { major: 226, minor }
}

fn test_output_key(minor: u32, connector_name: &str) -> OutputKey {
    OutputKey::new(test_device_key(minor), connector_name)
}

fn test_advertised_mode(
    width: u16,
    height: u16,
    vrefresh: u32,
    preferred: bool,
) -> crate::platform::drm::Mode {
    crate::platform::drm::Mode {
        name: format!("{width}x{height}"),
        width,
        height,
        vrefresh,
        preferred,
        ..Default::default()
    }
}

fn push_test_device(backend: &mut KmsBackend, key: DrmDeviceKey) {
    let device =
        std::rc::Rc::new(crate::drm::Device::for_tests().expect("open a second test DRM device"));
    backend
        .platform
        .devices
        .push(crate::kms::render::platform::KmsDevice {
            key,
            device,
            cursor: crate::kms::render::platform::KmsCursorState::new(),
        });
}

mod get_image_planes {
    use super::super::{
        apply_gc_function, apply_z_plane_mask, copy_area_clip_gpu_eligible, depth_plane_mask,
        read_z_pixmap_pixel, write_z_pixmap_pixel, z_to_xy_planes,
    };
    use yserver_core::backend::GcFunction;

    #[test]
    fn depth_plane_mask_truncates_to_depth() {
        assert_eq!(depth_plane_mask(1), 0x1);
        assert_eq!(depth_plane_mask(4), 0x0f);
        assert_eq!(depth_plane_mask(8), 0xff);
        assert_eq!(depth_plane_mask(24), 0x00ff_ffff);
        assert_eq!(depth_plane_mask(32), u32::MAX);
    }

    // Regression guard for the gkrellm/compositor fix (2026-06-20): a
    // clip-masked CopyArea only takes the fast GPU per-run blit for plain
    // GXcopy with a full plane-mask; everything else must fall back to the
    // CPU read-modify-write path. Routing GXcopy+full-mask to the GPU is
    // what removed gkrellm's per-run readback stall.
    #[test]
    fn copy_area_clip_gpu_eligible_only_for_gxcopy_full_mask() {
        use yserver_core::backend::GcFunction;
        let full = depth_plane_mask(24);
        // GXcopy + full plane-mask → GPU fast path (gkrellm's case).
        assert!(copy_area_clip_gpu_eligible(GcFunction::Copy, full, full));
        // Non-Copy rop → CPU read-modify-write path required.
        assert!(!copy_area_clip_gpu_eligible(GcFunction::Xor, full, full));
        assert!(!copy_area_clip_gpu_eligible(GcFunction::Invert, full, full));
        // Partial plane-mask → CPU path even for GXcopy.
        assert!(!copy_area_clip_gpu_eligible(
            GcFunction::Copy,
            full & !1,
            full
        ));
    }

    #[test]
    fn z_mask_depth24_zeroes_unrequested_planes() {
        // One BGRA pixel 0x00ff_80ff (LE bytes B=0xff G=0x80 R=0xff X=0).
        let mut px = vec![0xff, 0x80, 0xff, 0x00];
        apply_z_plane_mask(&mut px, 24, 0x0000_00ff); // blue planes only
        assert_eq!(px, vec![0xff, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn z_mask_depth8_masks_bytes() {
        let mut bytes = vec![0xab, 0x0f, 0xf0, 0x00];
        apply_z_plane_mask(&mut bytes, 8, 0x0f);
        assert_eq!(bytes, vec![0x0b, 0x0f, 0x00, 0x00]);
    }

    #[test]
    fn z_mask_depth4_masks_bytes() {
        // Depth-4 wire ZPixmap is nibble-PACKED (two pixels per
        // byte, low nibble first — see pack_from_storage), so the
        // mask applies to BOTH nibbles independently. The original
        // expectation here predated the a2d5480 nibble packing and
        // assumed one pixel per byte.
        // Full mask is the identity.
        let mut bytes = vec![0xab, 0x0f, 0xf0, 0x00];
        apply_z_plane_mask(&mut bytes, 4, 0x0f);
        assert_eq!(bytes, vec![0xab, 0x0f, 0xf0, 0x00]);
        // Partial mask 0b0011 keeps the low two planes of each pixel.
        let mut bytes = vec![0xab, 0x0f, 0xf0, 0x00];
        apply_z_plane_mask(&mut bytes, 4, 0x03);
        assert_eq!(bytes, vec![0x23, 0x03, 0x30, 0x00]);
    }

    #[test]
    fn xy_planes_depth4_unpacks_nibbles() {
        // pixel0=1, pixel1=2 packed low-nibble first into 0x21.
        let z = [0x21, 0x00, 0x00, 0x00];
        let out = z_to_xy_planes(&z, 2, 1, 4, 0x3);
        assert_eq!(out.len(), 2 * 4);
        // Plane 1 first: pixel1 only.
        assert_eq!(out[0], 0b0000_0010);
        // Plane 0 second: pixel0 only.
        assert_eq!(out[4], 0b0000_0001);
    }

    #[test]
    fn z_mask_depth1_empty_mask_zeroes() {
        let mut bytes = vec![0xff, 0xff, 0xff, 0xff];
        apply_z_plane_mask(&mut bytes, 1, 0);
        assert_eq!(bytes, vec![0, 0, 0, 0]);
    }

    #[test]
    fn xy_planes_msb_first_lsb_bit_order() {
        // 2x1 depth-24 image: pixel0 = 0x000001 (bit 0 set),
        // pixel1 = 0x800000 (bit 23 set). Request planes 23 and 0.
        let z = [
            0x01, 0x00, 0x00, 0x00, // pixel (0,0) BGRA LE
            0x00, 0x00, 0x80, 0x00, // pixel (1,0)
        ];
        let mask = (1 << 23) | 1;
        let out = z_to_xy_planes(&z, 2, 1, 24, mask);
        // Two planes, scanline pad 32 bits → 4 bytes per row.
        assert_eq!(out.len(), 2 * 4);
        // Plane 23 first (most significant): pixel1 → bit 1 LSBFirst.
        assert_eq!(out[0], 0b0000_0010);
        // Plane 0 second: pixel0 → bit 0.
        assert_eq!(out[4], 0b0000_0001);
    }

    #[test]
    fn xy_planes_depth1_is_identity_for_plane0() {
        // 40x2 depth-1 bitmap: stride = ceil(40/32)*4 = 8 bytes.
        let mut z = vec![0u8; 16];
        z[0] = 0xa5;
        z[9] = 0x3c;
        let out = z_to_xy_planes(&z, 40, 2, 1, 0x1);
        assert_eq!(out, z);
    }

    #[test]
    fn xy_planes_empty_mask_is_empty() {
        let z = [0u8; 16];
        assert!(z_to_xy_planes(&z, 2, 2, 24, 0).is_empty());
    }

    #[test]
    fn depth4_z_pixmap_pixel_round_trip_uses_nibbles() {
        let mut z = vec![0u8; 4];
        write_z_pixmap_pixel(&mut z, 4, 2, 0, 0, 0x1);
        write_z_pixmap_pixel(&mut z, 4, 2, 1, 0, 0xe);
        assert_eq!(z[0], 0xe1);
        assert_eq!(read_z_pixmap_pixel(&z, 4, 2, 0, 0), 0x1);
        assert_eq!(read_z_pixmap_pixel(&z, 4, 2, 1, 0), 0xe);
    }

    #[test]
    fn gc_function_respects_plane_mask() {
        assert_eq!(
            apply_gc_function(GcFunction::Copy, 0b1111, 0b0000, 0b0101),
            0b0101
        );
        assert_eq!(
            apply_gc_function(GcFunction::Invert, 0, 0b0011, 0b0001),
            0b0010
        );
        assert_eq!(
            apply_gc_function(GcFunction::Nor, 0b0001, 0b0010, 0b1111),
            0b1100
        );
    }
}

/// Place a live output on the fixture at `(x, y)` with a `width x height`
/// mode, and mark its registry entry connected and scanning out there —
/// the state a boot auto-layout session reaches with no client request.
fn push_enabled_test_output(
    b: &mut crate::kms::render::backend::KmsBackend,
    connector_name: &str,
    raw_crtc: u32,
    x: i32,
    y: i32,
    width: u16,
    height: u16,
) -> OutputKey {
    use crate::kms::backend::ActiveOutput;
    let device_key = b
        .platform
        .primary_device()
        .expect("test fixture has a DRM device")
        .key;
    let scanout_route = b
        .platform
        .scanout_route_for_kms(device_key)
        .expect("test fixture has a scanout route");
    let mode = test_advertised_mode(width, height, 60, true);
    let output = crate::platform::drm::Output {
        connector: ::drm::control::from_u32(raw_crtc).unwrap(),
        connector_name: connector_name.to_string(),
        encoder: ::drm::control::from_u32(raw_crtc).unwrap(),
        crtc: ::drm::control::from_u32(raw_crtc).unwrap(),
        plane: ::drm::control::from_u32(raw_crtc).unwrap(),
        // SAFETY: tests never pass this mode to DRM.
        mode: unsafe { std::mem::zeroed() },
        picked: mode.clone(),
        plane_fb_id_prop: ::drm::control::from_u32(1).unwrap(),
        plane_crtc_id_prop: ::drm::control::from_u32(1).unwrap(),
        plane_src_x_prop: ::drm::control::from_u32(1).unwrap(),
        plane_src_y_prop: ::drm::control::from_u32(1).unwrap(),
        plane_src_w_prop: ::drm::control::from_u32(1).unwrap(),
        plane_src_h_prop: ::drm::control::from_u32(1).unwrap(),
        plane_crtc_x_prop: ::drm::control::from_u32(1).unwrap(),
        plane_crtc_y_prop: ::drm::control::from_u32(1).unwrap(),
        plane_crtc_w_prop: ::drm::control::from_u32(1).unwrap(),
        plane_crtc_h_prop: ::drm::control::from_u32(1).unwrap(),
        plane_in_fence_fd_prop: None,
        crtc_out_fence_ptr_prop: None,
        scanout_modifiers: Vec::new(),
        mm_width: 0,
        mm_height: 0,
        edid: Vec::new(),
        connector_type: "unknown".to_string(),
        modes: vec![mode.clone()],
    };
    let active = ActiveOutput::new(
        scanout_route,
        output,
        crate::drm::Swapchain::empty_for_tests(),
        x,
        y,
    );
    let key = active.key.clone();
    b.platform.outputs.push(active);
    b.platform.scanout_pools.push(None);
    b.platform.bo_generations.push(Vec::new());
    b.platform.first_pageflip_logged.push(false);
    let entry = b.randr_id_alloc.entry_mut(&key);
    entry.connected = true;
    entry.modes = vec![mode];
    entry.config = crate::kms::render::backend::ConnectorConfig::Enabled {
        mode_w: width,
        mode_h: height,
        vrefresh: 60,
        x,
        y,
    };
    key
}

fn clear_test_outputs(b: &mut crate::kms::render::backend::KmsBackend) {
    b.platform.outputs.clear();
    b.platform.scanout_pools.clear();
    b.platform.bo_generations.clear();
    b.platform.first_pageflip_logged.clear();
}

fn test_cursor(b: &mut KmsBackend) -> u32 {
    use yserver_core::backend::{Backend, PixmapHandle};
    let pix = PixmapHandle::from_raw(0x1234_0040).unwrap();
    b.create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
        .expect("create_cursor")
        .as_raw()
}

/// Every XI2 event in `bytes` as (evtype, deviceid, sourceid, detail,
/// time) — raw and device events alike, in arrival order.
fn xi2_events(bytes: &[u8]) -> Vec<(u16, u16, u16, u32, u32)> {
    let rd16 = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let mut out = Vec::new();
    let mut i = 0;
    while i + 32 <= bytes.len() {
        if bytes[i] & 0x7f != 35 {
            i += 32;
            continue;
        }
        let event_len = 32 + 4 * rd32(i + 4) as usize;
        if i + event_len > bytes.len() {
            break;
        }
        let evtype = rd16(i + 8);
        // Raw events carry sourceid at byte 20, device events at 52.
        let sourceid = if (13..=17).contains(&evtype) {
            rd16(i + 20)
        } else {
            rd16(i + 52)
        };
        out.push((evtype, rd16(i + 10), sourceid, rd32(i + 16), rd32(i + 12)));
        i += event_len;
    }
    out
}

fn kbd_map_drain_until(
    peer: &mut std::os::unix::net::UnixStream,
    ready: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut bytes = Vec::new();
    loop {
        bytes.extend(kbd_map_drain(peer));
        if ready(&bytes) || std::time::Instant::now() >= deadline {
            return bytes;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn dynamic_test_device(
    source_id: yserver_core::xinput::InputSourceId,
    keyboard: bool,
    pointer: bool,
) -> yserver_core::core_loop::DeviceInfo {
    yserver_core::core_loop::DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: yserver_core::xinput::InputCapabilities {
            keyboard,
            pointer,
            touch: false,
        },
        name: format!("dynamic test device {}", source_id.0),
        device_node: format!("/dev/input/event{}", source_id.0),
        sysname: format!("event{}", source_id.0),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: yserver_core::core_loop::message::LibinputConfigSnapshot::default(),
    }
}

fn process_dynamic_test_keyboard_grab(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    client: u32,
    device_id: u16,
    sequence: u16,
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[1, 1, 0, 0]); // async device and paired modes, owner_events=false
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&((1u32 << 2) | (1u32 << 3)).to_le_bytes()); // KeyPress/Release
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        yserver_protocol::x11::ClientId(client),
        yserver_protocol::x11::SequenceNumber(sequence),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 51,
            length_units: 7,
        },
        &body,
        None,
    )
    .expect("XIGrabDevice on dynamic keyboard facet");
}

fn process_dynamic_test_keyboard_ungrab(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    client: u32,
    device_id: u16,
    sequence: u16,
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // time
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        yserver_protocol::x11::ClientId(client),
        yserver_protocol::x11::SequenceNumber(sequence),
        yserver_protocol::x11::RequestHeader {
            opcode: 137,
            data: 52,
            length_units: 3,
        },
        &body,
        None,
    )
    .expect("XIUngrabDevice on dynamic keyboard facet");
}

pub(super) fn seed_window(
    b: &mut KmsBackend,
    xid: u32,
    parent: Option<u32>,
    x: i16,
    y: i16,
) -> crate::kms::render::store::DrawableId {
    use crate::kms::render::store::{DrawableKind, Storage};
    b.windows.insert(
        xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: 100,
            height: 100,
            // Normal (non-ARGB) X windows are depth 24; the redirect-routing
            // tests rely on resolve_paint_target reporting this logical depth
            // even when routed into a depth-32 backing (the picom/xterm shape).
            depth: 24,
            mapped: true,
            viewable: true,
            parent,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.store
        .allocate(
            xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("seed_window allocate")
}

/// #133 step 3 — `seed_window` with a border and an explicit size,
/// sized like `allocate_window_storage` would: storage is the
/// BORDERED extent `(w + 2bw) x (h + 2bw)` (Xorg `compAllocPixmap`,
/// `composite/compalloc.c:610`).
fn seed_bordered_window(
    b: &mut KmsBackend,
    xid: u32,
    parent: Option<u32>,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    bw: u16,
) -> crate::kms::render::store::DrawableId {
    use crate::kms::render::store::{DrawableKind, Storage};
    b.windows.insert(
        xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: bw,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: w,
            height: h,
            depth: 24,
            mapped: true,
            viewable: true,
            parent,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    let (sw, sh) = crate::kms::render::backend::bordered_storage_extent(w.max(1), h.max(1), bw);
    let id = b
        .store
        .allocate(
            xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: sw,
                    height: sh,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("seed_bordered_window allocate");
    // Mirror production: the layout a storage was allocated with is
    // recorded on the drawable, never re-derived from the live
    // geometry (`Drawable::content_offset`).
    b.store.set_content_offset(id, i32::from(bw));
    id
}

fn rect(x: i16, y: i16, width: u16, height: u16) -> Rectangle16 {
    Rectangle16 {
        x,
        y,
        width,
        height,
    }
}

/// Synthesise a deterministic host xid for a nested `ResourceId`.
/// Mirrors the production "high bit set" convention used by the
/// sibling core tests so the v2 windows keys never collide
/// with low-numbered nested xids.
fn synth_host_xid(xid: yserver_protocol::x11::ResourceId) -> u32 {
    0x8000_0000 | xid.0
}

/// Seed both the resource-layer `Window` *and* the v2 backend's
/// `windows` + `store` entries so that:
/// - `state.resources.window(xid).host_xid` is set,
/// - `backend.windows[host_xid]` exists with the requested
///   geometry, and
/// - `backend.store.lookup(host_xid)` returns a real
///   `DrawableId`.
fn seed_state_window(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    xid: yserver_protocol::x11::ResourceId,
    parent: yserver_protocol::x11::ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::{ClientId, CreateWindowRequest};
    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            depth: 24,
            window: xid,
            parent,
            x,
            y,
            width,
            height,
            border_width: 0,
            class: 1,
            visual: yserver_core::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let host_xid = synth_host_xid(xid);
    if let Some(w) = state.resources.window_mut(xid) {
        w.host_xid = yserver_core::backend::WindowHandle::from_raw(host_xid);
    }
    // v2 backend's windows + store mirror.
    let parent_host = if parent == ROOT_WINDOW {
        None
    } else {
        Some(synth_host_xid(parent))
    };
    backend.windows.insert(
        host_xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width,
            height,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: parent_host,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    let _ = backend.store.allocate(
        host_xid,
        crate::kms::render::store::DrawableKind::Window,
        24,
        true,
        crate::kms::render::store::Storage::for_tests_null(
            ash::vk::Extent2D {
                width: u32::from(width.max(1)),
                height: u32::from(height.max(1)),
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        ),
    );
}

/// Drive ReparentWindow through the public `process_request`
/// dispatcher (the v2 backend can't call `handle_reparent_window`
/// directly — it's `fn`-private to the core_loop module).
fn dispatch_reparent_window(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    window: yserver_protocol::x11::ResourceId,
    parent: yserver_protocol::x11::ResourceId,
    x: i16,
    y: i16,
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&parent.0.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());

    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 7, // ReparentWindow
            data: 0,
            length_units: 4,
        },
        &body,
        None,
    )
    .expect("process_request must succeed");
}

fn dispatch_configure_window(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    window: yserver_protocol::x11::ResourceId,
    x: Option<i16>,
    y: Option<i16>,
    border_width: Option<u16>,
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut mask = 0u16;
    if x.is_some() {
        mask |= 1 << 0;
    }
    if y.is_some() {
        mask |= 1 << 1;
    }
    if border_width.is_some() {
        mask |= 1 << 4;
    }

    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    if let Some(x) = x {
        body.extend_from_slice(&(i32::from(x) as u32).to_le_bytes());
    }
    if let Some(y) = y {
        body.extend_from_slice(&(i32::from(y) as u32).to_le_bytes());
    }
    if let Some(border_width) = border_width {
        body.extend_from_slice(&u32::from(border_width).to_le_bytes());
    }

    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 12, // ConfigureWindow
            data: 0,
            length_units: u32::try_from((4 + body.len()) / 4).expect("request length"),
        },
        &body,
        None,
    )
    .expect("process_request(ConfigureWindow) must succeed");
}

fn create_live_window(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    xid: yserver_protocol::x11::ResourceId,
    parent: yserver_protocol::x11::ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> yserver_core::backend::WindowHandle {
    use yserver_core::{
        backend::Backend,
        host_x11::HostSubwindowVisual,
        resources::{ROOT_VISUAL, ROOT_WINDOW},
    };
    use yserver_protocol::x11::ClientId;

    state.resources.create_window(
        ClientId(14),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: xid,
            parent,
            x,
            y,
            width,
            height,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );

    let host_parent = if parent == ROOT_WINDOW {
        yserver_core::backend::WindowHandle::from_raw(backend.core.window_id).expect("root")
    } else {
        state
            .resources
            .window(parent)
            .and_then(|w| w.host_xid)
            .expect("parent host_xid")
    };
    let host = backend
        .create_subwindow(
            None,
            host_parent,
            x,
            y,
            width,
            height,
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
    state.resources.window_mut(xid).expect("window").host_xid = Some(host);
    if parent == ROOT_WINDOW {
        backend
            .register_top_level(None, xid, host.as_raw())
            .expect("register_top_level");
    } else {
        backend
            .register_subwindow(None, xid, host.as_raw())
            .expect("register_subwindow");
    }
    let _ = state.resources.map_window(xid);
    backend
        .map_window_for_tests(host.as_raw())
        .expect("map_subwindow");
    host
}

/// Retain `source_xid` as a fully-retired direct frame on every output —
/// the state `try_present_direct` reaches after its flip retires, without
/// the DRM transaction. `source_xid` stands in for the client buffer the
/// CRTCs are scanning out; the compositor's pools are left untouched,
/// which is exactly what happens on hardware.
fn retain_direct_frame_from_source_test(
    b: &mut crate::kms::render::backend::KmsBackend,
    source_xid: u32,
    fallback_xid: u32,
    width: u16,
    height: u16,
) {
    use yserver_core::backend::{
        CompletedPresentEvent, PresentClockSample, PresentClockSource, PresentScanoutCandidate,
        PresentWake,
    };

    let source_id = b.store.lookup(source_xid).expect("direct source drawable");
    let fallback_id = b.store.lookup(fallback_xid).expect("fallback drawable");
    let source_pin = b.pin_direct_source(source_id);
    let fallback_target_pin = b.pin_direct_source(fallback_id);
    let completion_clock = Some(PresentClockSample {
        msc: 3,
        ust: 300,
        source: PresentClockSource::PageFlip,
    });
    b.scanout_m2.current = Some(crate::kms::render::backend::DirectPresentFrame {
        source_pin,
        fallback_target_pin,
        source_id,
        candidate: PresentScanoutCandidate {
            client_id: 1,
            present_id: 3,
            crtc_id: 0,
            crtc_epoch: 0,
            src_pixmap_xid: source_xid,
            dst_window_xid: fallback_xid,
            src_host_xid: source_xid,
            paint_dst_host_xid: fallback_xid,
            completion_dst_host_xid: fallback_xid,
            src_width: width,
            src_height: height,
            x_off: 0,
            y_off: 0,
            valid_region_xid: 0,
            update_region_xid: 0,
            update_is_full: true,
            explicit_sync: false,
            options: 0,
        },
        fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
        event: CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(1),
            serial: 1,
            host_xid: source_xid,
            dst_host_xid: fallback_xid,
            options: 0,
            present_id: 3,
            window_generation: 1,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_FLIP,
            emit_idle: false,
        },
        completion_output_idx: 0,
        completion_clock,
        awaiting_outputs: std::collections::HashSet::new(),
    });
    b.scanout_m2.hold_direct = true;
}

fn dispatch_raw(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from((4 + body.len()) / 4).expect("request length"),
        },
        body,
        None,
    )
    .expect("process_request must succeed");
}

fn r(x: i32, y: i32, w: u32, h: u32) -> ash::vk::Rect2D {
    ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D {
            width: w,
            height: h,
        },
    }
}

/// Register a client in `state.clients` so `process_request`'s
/// per-client sequence stamp + (potential) error-emission path
/// have a real `ClientState` to operate on. Uses
/// `resource_id_mask = u32::MAX` so the fixture xids are
/// trivially in-range.
pub(super) fn install_client_for_render(state: &mut yserver_core::server::ServerState, id: u32) {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_core::{resources::ROOT_WINDOW, server::ClientState};
    use yserver_protocol::x11::ClientByteOrder;
    let (a, _b) = UnixStream::pair().unwrap();
    state.clients.insert(
        id,
        ClientState {
            writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(a))),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
}

/// Push a second live output onto the fixture, with its own selected raw
/// CRTC handle. Mirrors `PlatformBackend::for_tests()`'s single-output
/// literal.
pub(super) fn push_test_output(b: &mut crate::kms::render::backend::KmsBackend, crtc_id: u32) {
    use crate::kms::backend::ActiveOutput;
    let device_key = b
        .platform
        .primary_device()
        .expect("test fixture has a DRM device")
        .key;
    let scanout_route = b
        .platform
        .scanout_route_for_kms(device_key)
        .expect("test fixture has a scanout route");
    b.platform.outputs.push(ActiveOutput::new(
        scanout_route,
        crate::platform::drm::Output {
            connector: ::drm::control::from_u32(crtc_id).unwrap(),
            connector_name: "test2".to_string(),
            encoder: ::drm::control::from_u32(crtc_id).unwrap(),
            crtc: ::drm::control::from_u32(crtc_id).unwrap(),
            plane: ::drm::control::from_u32(crtc_id).unwrap(),
            // SAFETY: tests never pass this mode to DRM.
            mode: unsafe { std::mem::zeroed() },
            picked: crate::platform::drm::Mode {
                name: "test2".to_string(),
                width: 800,
                height: 600,
                vrefresh: 60,
                preferred: true,
                ..Default::default()
            },
            plane_fb_id_prop: ::drm::control::from_u32(1).unwrap(),
            plane_crtc_id_prop: ::drm::control::from_u32(1).unwrap(),
            plane_src_x_prop: ::drm::control::from_u32(1).unwrap(),
            plane_src_y_prop: ::drm::control::from_u32(1).unwrap(),
            plane_src_w_prop: ::drm::control::from_u32(1).unwrap(),
            plane_src_h_prop: ::drm::control::from_u32(1).unwrap(),
            plane_crtc_x_prop: ::drm::control::from_u32(1).unwrap(),
            plane_crtc_y_prop: ::drm::control::from_u32(1).unwrap(),
            plane_crtc_w_prop: ::drm::control::from_u32(1).unwrap(),
            plane_crtc_h_prop: ::drm::control::from_u32(1).unwrap(),
            plane_in_fence_fd_prop: None,
            crtc_out_fence_ptr_prop: None,
            scanout_modifiers: Vec::new(),
            mm_width: 0,
            mm_height: 0,
            edid: Vec::new(),
            connector_type: "unknown".to_string(),
            modes: vec![crate::platform::drm::Mode {
                name: "test2".to_string(),
                width: 800,
                height: 600,
                vrefresh: 60,
                preferred: true,
                ..Default::default()
            }],
        },
        crate::drm::Swapchain::empty_for_tests(),
        800,
        0,
    ));
}

pub(super) fn install_direct_frame_for_target_test(
    b: &mut crate::kms::render::backend::KmsBackend,
    target_xid: u32,
    fallback_id: crate::kms::render::store::DrawableId,
    current: bool,
) -> (
    crate::kms::render::store::DrawableId,
    crate::kms::render::store::DrawableId,
    u64,
    u64,
) {
    use ash::vk;
    use yserver_core::backend::{
        CompletedPresentEvent, PresentClockSample, PresentClockSource, PresentScanoutCandidate,
        PresentWake,
    };

    use crate::kms::render::store::{DrawableKind, Storage};

    let source_xid = target_xid + 1;
    let source_id = b
        .store
        .allocate(
            source_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("direct source");
    let source_pin = b.pin_direct_source(source_id);
    let fallback_target_pin = b.pin_direct_source(fallback_id);
    let completion_clock = current.then_some(PresentClockSample {
        msc: 17,
        ust: 1_700,
        source: PresentClockSource::PageFlip,
    });
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        present_id: 77,
        crtc_id: 0,
        crtc_epoch: 0,
        src_pixmap_xid: source_xid,
        dst_window_xid: target_xid,
        src_host_xid: source_xid,
        paint_dst_host_xid: target_xid,
        completion_dst_host_xid: target_xid,
        src_width: 100,
        src_height: 100,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    let frame = crate::kms::render::backend::DirectPresentFrame {
        source_pin,
        fallback_target_pin,
        source_id,
        candidate,
        fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
        event: CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(1),
            serial: 9,
            host_xid: source_xid,
            dst_host_xid: target_xid,
            options: 0,
            present_id: 77,
            window_generation: 5,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: if current {
                yserver_protocol::x11::present::COMPLETE_MODE_FLIP
            } else {
                yserver_protocol::x11::present::COMPLETE_MODE_COPY
            },
            emit_idle: !current,
        },
        completion_output_idx: 0,
        completion_clock,
        awaiting_outputs: if current {
            std::collections::HashSet::new()
        } else {
            std::collections::HashSet::from([0])
        },
    };
    if current {
        b.scanout_m2.current = Some(frame);
    } else {
        b.scanout_m2.pending = Some(frame);
    }
    b.scanout_m2.hold_direct = true;
    (source_id, fallback_id, source_pin, fallback_target_pin)
}

fn kbd_map_client(state: &mut yserver_core::server::ServerState) -> std::os::unix::net::UnixStream {
    kbd_map_client_id(state, 5)
}

fn kbd_map_client_id(
    state: &mut yserver_core::server::ServerState,
    id: u32,
) -> std::os::unix::net::UnixStream {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    let (peer, writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    state.clients.insert(
        id,
        yserver_core::server::ClientState {
            writer: Arc::new(Mutex::new(yserver_core::transport::Transport::Unix(writer))),
            byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0,
            resource_id_mask: u32::MAX,
            event_masks: HashMap::new(),
            save_set: HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: HashMap::new(),
            xi1_event_classes: HashSet::new(),
            xi1_window_event_classes: HashMap::new(),
            outbound: VecDeque::new(),
            watching_writable: false,
            write_failed: false,
            focused_window: yserver_core::resources::ROOT_WINDOW,
            reader_control: None,
            is_local: true,
            fd_passing: true,
        },
    );
    peer
}

fn kbd_map_drain(peer: &mut std::os::unix::net::UnixStream) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    while let Ok(n) = peer.read(&mut buf) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn kbd_map_request(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        yserver_protocol::x11::ClientId(5),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

/// ChangeKeyboardMapping { first, kpk, count, syms } as the wire body after the header.
fn change_kbd_map_body(first: u8, kpk: u8, syms: &[u32]) -> Vec<u8> {
    let mut body = vec![first, kpk, 0, 0];
    for s in syms {
        body.extend_from_slice(&s.to_le_bytes());
    }
    body
}

fn kbd_map_backend(layout: &str, options: Option<&str>) -> KmsBackend {
    let mut backend = KmsBackend::for_tests();
    backend.core.install_keymap(
        &crate::kms::xkb::golden_context(),
        crate::kms::xkb::golden_keymap(layout, options),
        &crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: layout.into(),
            variant: String::new(),
            options: options.map(str::to_owned),
        },
    );
    backend
}

/// One key of an XKB GetMap, as `testdata/xorg-xkb-change-keyboard-mapping.txt`
/// prints it (and as [`decode_xkb_get_map`] reads our reply).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct XkbKeyRow {
    kt: [u8; 4],
    gi: u8,
    width: u8,
    syms: Vec<u32>,
    acts: Vec<[u8; 8]>,
    beh: (u8, u8),
    expl: u8,
    mm: u8,
    vmm: u16,
}

fn parse_xkb_key_row(line: &str) -> (u8, XkbKeyRow) {
    let mut it = line[2..].split(' ');
    let kc: u8 = it.next().unwrap().parse().unwrap();
    let mut row = XkbKeyRow::default();
    let hex = |v: &str| u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap();
    for field in it {
        let (k, v) = field.split_once('=').unwrap();
        match k {
            "kt" => {
                for (i, t) in v.split(',').enumerate() {
                    row.kt[i] = t.parse().unwrap();
                }
            }
            "gi" => row.gi = u8::try_from(hex(v)).unwrap(),
            "w" => row.width = v.parse().unwrap(),
            "syms" if v == "-" => {}
            "syms" => row.syms = v.split(',').map(hex).collect(),
            "acts" if v == "-" => {}
            "acts" => {
                row.acts = v
                    .split(',')
                    .map(|a| {
                        let mut b = [0u8; 8];
                        for (i, byte) in b.iter_mut().enumerate() {
                            *byte = u8::from_str_radix(&a[2 * i..2 * i + 2], 16).unwrap();
                        }
                        b
                    })
                    .collect();
            }
            "beh" => {
                let (t, d) = v.split_once(':').unwrap();
                row.beh = (
                    u8::from_str_radix(t, 16).unwrap(),
                    u8::from_str_radix(d, 16).unwrap(),
                );
            }
            "expl" => row.expl = u8::try_from(hex(v)).unwrap(),
            "mm" => row.mm = u8::try_from(hex(v)).unwrap(),
            "vmm" => row.vmm = u16::try_from(hex(v)).unwrap(),
            other => panic!("unknown key field {other}"),
        }
    }
    (kc, row)
}

/// One request from client `client` through the core loop.
fn xkb_client_request(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    client: u32,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        yserver_protocol::x11::ClientId(client),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

/// A request of `testdata/xorg-xkb-set-modifier-mapping.txt`.
#[derive(Clone, Debug)]
enum SmmRequest {
    Ckm {
        first: u8,
        kpk: u8,
        count: u8,
        syms: Vec<u32>,
    },
    /// SetModifierMapping with exactly these keycodes (`smm`, or the
    /// `sent` line of an `smmx`).
    Smm {
        kpm: u8,
        keys: Vec<u8>,
    },
    Down(u8),
    Up(u8),
    Run(String),
}

/// What Xorg answered.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SmmResult {
    Ok,
    Status(u8),
    Error(u8, u32),
    Exit,
}

#[derive(Clone, Debug)]
struct SmmStep {
    request: SmmRequest,
    result: SmmResult,
    /// The XKB listener's events: the core keyboard's (dev 3) XKB events
    /// and its core events, in arrival order.
    listener: Vec<Vec<u8>>,
    /// The plain connection's events.
    plain: Vec<Vec<u8>>,
    repeats: Vec<(u8, u8, u8)>,
    before: std::collections::BTreeMap<u8, XkbKeyRow>,
    after: std::collections::BTreeMap<u8, XkbKeyRow>,
    /// `+vmods`: the virtual modifier table afterwards, when it changed.
    vmods_after: Option<Vec<u8>>,
    /// `-vmods`: the virtual modifier table before, when it changed.
    vmods_before: Option<Vec<u8>>,
    /// `-type N` / `+type N`: the key types that changed, before and
    /// after, by Xorg index.
    types_before: Vec<(usize, String)>,
    types_after: Vec<(usize, String)>,
    /// GetModifierMapping afterwards: (kpm, keycodes row by row).
    coremodmap: (u8, Vec<u8>),
}

struct SmmCase {
    name: String,
    layout: String,
    steps: Vec<SmmStep>,
}

fn parse_xkb_smm_golden(text: &str) -> Vec<SmmCase> {
    let field = |line: &str, key: &str| -> Option<String> {
        line.split(' ')
            .find_map(|t| t.strip_prefix(&format!("{key}=")).map(str::to_owned))
    };
    let raw = |line: &str| -> Vec<u8> {
        let h = field(line, "raw").unwrap();
        (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    let list = |v: &str| -> Vec<u8> {
        v.split(',')
            .filter(|s| !s.is_empty())
            .map(|k| k.parse().unwrap())
            .collect()
    };
    let mut cases: Vec<SmmCase> = Vec::new();
    let mut in_total = false;
    for line in text.lines() {
        if line.starts_with('#') && !line.starts_with("## ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            cases.push(SmmCase {
                name: rest.split(' ').next().unwrap().to_owned(),
                layout: field(line, "layout").unwrap(),
                steps: Vec::new(),
            });
            in_total = false;
            continue;
        }
        if line == "> total" {
            in_total = true;
            continue;
        }
        if in_total {
            continue;
        }
        if let Some(req) = line.strip_prefix("> ") {
            let request = if let Some(r) = req.strip_prefix("ckm:") {
                let mut it = r.splitn(4, ':');
                SmmRequest::Ckm {
                    first: it.next().unwrap().parse().unwrap(),
                    kpk: it.next().unwrap().parse().unwrap(),
                    count: it.next().unwrap().parse().unwrap(),
                    syms: it
                        .next()
                        .unwrap()
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(|h| u32::from_str_radix(h, 16).unwrap())
                        .collect(),
                }
            } else if let Some(r) = req.strip_prefix("smm:") {
                let (kpm, keys) = r.split_once(':').unwrap();
                SmmRequest::Smm {
                    kpm: kpm.parse().unwrap(),
                    keys: list(keys),
                }
            } else if req.starts_with("smmx:") {
                // Filled in by the `sent` line.
                SmmRequest::Smm {
                    kpm: 0,
                    keys: Vec::new(),
                }
            } else if let Some(kc) = req.strip_prefix("down:") {
                SmmRequest::Down(kc.parse().unwrap())
            } else if let Some(kc) = req.strip_prefix("up:") {
                SmmRequest::Up(kc.parse().unwrap())
            } else if let Some(cmd) = req.strip_prefix("run:") {
                SmmRequest::Run(cmd.to_owned())
            } else {
                panic!("unparsed golden request: {line}");
            };
            cases.last_mut().unwrap().steps.push(SmmStep {
                request,
                result: SmmResult::Ok,
                listener: Vec::new(),
                plain: Vec::new(),
                repeats: Vec::new(),
                before: std::collections::BTreeMap::new(),
                after: std::collections::BTreeMap::new(),
                vmods_after: None,
                vmods_before: None,
                types_before: Vec::new(),
                types_after: Vec::new(),
                coremodmap: (0, Vec::new()),
            });
            continue;
        }
        let step = cases.last_mut().unwrap().steps.last_mut().unwrap();
        if let Some(sent) = line.strip_prefix("sent ") {
            step.request = SmmRequest::Smm {
                kpm: field(sent, "kpm").unwrap().parse().unwrap(),
                keys: list(&field(sent, "keys").unwrap()),
            };
        } else if line == "= ok" {
            step.result = SmmResult::Ok;
        } else if line.starts_with("= exit=") {
            step.result = SmmResult::Exit;
        } else if let Some(st) = line.strip_prefix("= status=") {
            step.result = SmmResult::Status(st.parse().unwrap());
        } else if line.starts_with("= error=") {
            step.result = SmmResult::Error(
                field(line, "error").unwrap().parse().unwrap(),
                field(line, "value").unwrap().parse().unwrap(),
            );
        } else if line.starts_with("e xkb ") {
            if field(line, "dev").as_deref() == Some("3") {
                step.listener.push(raw(line));
            }
        } else if line.starts_with("e xkbl ") {
            step.listener.push(raw(line));
        } else if line.starts_with("e core ") {
            step.plain.push(raw(line));
        } else if let Some(r) = line.strip_prefix("repeat ") {
            let (kc, change) = r.split_once(' ').unwrap();
            let (a, b) = change.split_once("->").unwrap();
            step.repeats
                .push((kc.parse().unwrap(), a.parse().unwrap(), b.parse().unwrap()));
        } else if let Some(v) = line.strip_prefix("+vmods ") {
            step.vmods_after = Some(
                v.split(',')
                    .map(|h| u8::from_str_radix(h, 16).unwrap())
                    .collect(),
            );
        } else if let Some(v) = line.strip_prefix("-vmods ") {
            step.vmods_before = Some(
                v.split(',')
                    .map(|h| u8::from_str_radix(h, 16).unwrap())
                    .collect(),
            );
        } else if let Some(t) = line.strip_prefix("-type ") {
            let (n, rest) = t.split_once(' ').unwrap();
            step.types_before
                .push((n.parse().unwrap(), rest.to_owned()));
        } else if let Some(t) = line.strip_prefix("+type ") {
            let (n, rest) = t.split_once(' ').unwrap();
            step.types_after.push((n.parse().unwrap(), rest.to_owned()));
        } else if line.starts_with("- ") {
            let (kc, row) = parse_xkb_key_row(line);
            step.before.insert(kc, row);
        } else if line.starts_with("+ ") {
            let (kc, row) = parse_xkb_key_row(line);
            step.after.insert(kc, row);
        } else if let Some(m) = line.strip_prefix("coremodmap ") {
            let mut it = m.split(' ');
            let kpm = field(it.next().unwrap(), "kpm").unwrap().parse().unwrap();
            let keys = it
                .flat_map(|row| list(row.split_once(':').unwrap().1))
                .collect();
            step.coremodmap = (kpm, keys);
        } else {
            panic!("unparsed golden line: {line}");
        }
    }
    cases
}

fn get_modifier_mapping_reply(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
) -> (u8, Vec<u8>) {
    kbd_map_request(state, backend, 119, 0, &[]);
    let r = kbd_map_drain(peer);
    assert_eq!(r[0], 1, "GetModifierMapping reply");
    (r[1], r[32..32 + 8 * usize::from(r[1])].to_vec())
}

fn host_key(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    kc: u8,
    pressed: bool,
) {
    use yserver_core::backend::Backend;
    backend.on_host_input(
        state,
        yserver_core::core_loop::message::HostInputEvent::Key(
            yserver_core::host_x11::HostKeyEvent {
                origin: yserver_core::core_loop::InputOrigin::NestedHost,
                keycode: kc,
                pressed,
                state: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                time: 0,
            },
        ),
    );
}

fn xi_xtest_grab_request(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    use yserver_core::{backend::Backend, core_loop::client_io};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    const CLIENT: u32 = 5;
    let outcome = yserver_core::core_loop::process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(CLIENT),
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("request through the core dispatcher");
    assert!(
        matches!(
            outcome,
            yserver_core::core_loop::process_request::RequestOutcome::Handled
        ),
        "request outcome: {outcome:?}"
    );
    for _ in 0..100 {
        if client_io::drain_outbound(state.clients.get_mut(&CLIENT).unwrap()).unwrap()
            == client_io::WriteOutcome::Done
        {
            break;
        }
    }
    kbd_map_drain(peer)
}

fn xi_xtest_ungrab_body(device_id: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body
}

fn xi_xtest_grab_reply_status(wire: &[u8]) -> Option<u8> {
    let mut offset = 0;
    while offset < wire.len() {
        let event_type = *wire.get(offset)?;
        let extra_units = usize::try_from(u32::from_le_bytes(
            wire.get(offset + 4..offset + 8)?.try_into().ok()?,
        ))
        .ok()?;
        let length = 32usize.checked_add(extra_units.checked_mul(4)?)?;
        if event_type == 1 {
            return wire.get(offset + 8).copied();
        }
        if event_type != 35 || offset.checked_add(length)? > wire.len() {
            return None;
        }
        offset += length;
    }
    None
}

type XiXtestRegistrySnapshot = Vec<(
    u16,
    Option<yserver_core::xinput::XiDeviceRole>,
    bool,
    Option<u16>,
    u16,
)>;

type XiXtestPropertySnapshot = Vec<(
    u16,
    std::collections::BTreeMap<yserver_protocol::x11::AtomId, yserver_core::xinput::XiProperty>,
)>;

type XiXtestHeldSnapshot = (
    [u8; 32],
    u16,
    std::collections::HashMap<
        u16,
        std::collections::HashMap<u8, yserver_core::core_loop::InputOrigin>,
    >,
    std::collections::HashSet<u8>,
    u16,
);

fn xi_xtest_registry_snapshot(state: &ServerState) -> XiXtestRegistrySnapshot {
    state
        .xi_devices
        .devices()
        .iter()
        .map(|device| {
            (
                device.id,
                state.xi_devices.role(device.id),
                device.enabled,
                device.attached_master,
                device.buttons_down,
            )
        })
        .collect()
}

fn xi_xtest_property_snapshot(state: &ServerState) -> XiXtestPropertySnapshot {
    state
        .xi_devices
        .devices()
        .iter()
        .map(|device| (device.id, device.properties.clone()))
        .collect()
}

fn xi_xtest_held_snapshot(state: &ServerState, backend: &KmsBackend) -> XiXtestHeldSnapshot {
    (
        state.keys_down,
        state.buttons_down,
        state.key_down_by_device.clone(),
        backend.core.down_keys.clone(),
        backend.core.button_mask,
    )
}
