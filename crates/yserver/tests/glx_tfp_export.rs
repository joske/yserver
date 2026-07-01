//! GLX texture-from-pixmap: exportable-image allocation + dma-buf export.
//!
//! Vulkan-gated — run with:
//!   cargo test --test glx_tfp_export -- --ignored

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;

use ash::vk;
use yserver::kms::{
    v2::KmsBackendV2,
    vk::{device::VkContext, dri3::DmabufExport},
};
use yserver_core::backend::Backend;

#[test]
#[ignore = "requires a Vulkan device"]
fn allocate_exportable_yields_valid_dmabuf_fd() {
    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run on HW");

    let img = yserver::kms::vk::target::allocate_exportable(
        &vk,
        /* width */ 64,
        /* height */ 32,
        yserver::kms::vk::target::EXPORT_FORMAT_BGRA8,
    )
    .expect("allocate exportable image");

    // Stride/size from vkGetImageSubresourceLayout must be sane.
    assert!(
        img.stride >= 64 * 4,
        "stride {} too small for 64px BGRA8 row",
        img.stride
    );
    assert!(
        img.size as usize >= img.stride as usize * 32,
        "size {} too small for {}*32",
        img.size,
        img.stride
    );

    // Export must succeed and carry sane stride/size.
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).expect("export_backing failed");
    assert!(
        export.stride >= 64 * 4,
        "export stride {} too small",
        export.stride
    );
    assert!(
        export.size >= export.stride * 32,
        "export size {} too small for stride {} * 32 rows",
        export.size,
        export.stride
    );
}

// ───────────────────────────────────────────────────────────────────
// Task 1.2: pixmap promotion liveness test + harness
// ───────────────────────────────────────────────────────────────────

/// Minimal engine/store/platform harness built atop the production
/// `KmsBackendV2` (the only construction path that wires a real
/// engine + store + platform with a live VkContext). All drawable
/// manipulation goes through the public `Backend` trait + the
/// `*_for_tests` shims, so this harness never reaches into the
/// crate-private engine internals directly.
struct PromoteHarness {
    backend: KmsBackendV2,
    vk: Arc<VkContext>,
}

impl PromoteHarness {
    /// Create a depth-`depth` pixmap of `w`×`h`. Returns the host xid.
    fn create_pixmap(&mut self, w: u16, h: u16, depth: u8) -> u32 {
        let handle = self
            .backend
            .create_pixmap(None, depth, w, h)
            .expect("create_pixmap");
        handle.as_raw()
    }

    /// Fill the whole pixmap with a solid 0xRRGGBB colour (the X11
    /// foreground convention used by `poly_fill_rectangle`: an ARGB
    /// pixel, with alpha forced opaque).
    fn fill_solid(&mut self, xid: u32, rgb: u32, w: u16, h: u16) {
        let mut rect = Vec::new();
        rect.extend_from_slice(&i16::to_le_bytes(0)); // x
        rect.extend_from_slice(&i16::to_le_bytes(0)); // y
        rect.extend_from_slice(&u16::to_le_bytes(w)); // w
        rect.extend_from_slice(&u16::to_le_bytes(h)); // h
        self.backend
            .poly_fill_rectangle(None, xid, 0xFF00_0000 | rgb, &rect)
            .expect("poly_fill_rectangle");
    }

    /// Close any open frame batch and wait on every in-flight fence so
    /// the GPU has actually landed all queued paints.
    fn flush_and_wait(&mut self) {
        self.backend
            .engine_close_open_frame_for_timeout_for_tests()
            .expect("close open frame");
        self.backend.engine_drain_all_for_tests();
    }

    /// Promote the pixmap onto exportable storage and export the
    /// resulting dma-buf.
    fn promote_and_export(&mut self, xid: u32) -> std::io::Result<DmabufExport> {
        self.backend.promote_and_export_pixmap_for_tests(xid)
    }
}

/// Build the harness, or `None` if no Vulkan device is present.
fn test_engine_harness() -> Option<PromoteHarness> {
    let backend = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return None;
        }
    };
    let vk = backend.test_vk_arc().expect("vk arc present");
    Some(PromoteHarness { backend, vk })
}

/// Read one BGRA pixel (as 0xRRGGBB, alpha dropped) out of the exported
/// dma-buf. Re-imports the fd through the production
/// `DrawableImage::from_dmabuf` path, then `vkCmdCopyImageToBuffer`s
/// into a HOST_VISIBLE staging buffer — DEVICE_LOCAL exported memory
/// is not CPU-mappable on a dGPU, so a raw mmap of the fd is not used.
fn read_dmabuf_pixel(
    vk: &Arc<VkContext>,
    exported: &DmabufExport,
    w: u32,
    h: u32,
    x: u32,
    y: u32,
) -> u32 {
    use std::os::fd::AsFd;
    use yserver::kms::vk::target::{DrawableImage, EXPORT_FORMAT_BGRA8};

    // Re-import: dup the fd so the original `exported.fd` stays owned
    // by the caller.
    let dup_fd = exported.fd.as_fd().try_clone_to_owned().expect("dup fd");
    let img = DrawableImage::from_dmabuf(
        Arc::clone(vk),
        dup_fd,
        w,
        h,
        EXPORT_FORMAT_BGRA8,
        exported.modifier,
        &[u64::from(exported.offset)],
        &[exported.stride],
    )
    .expect("from_dmabuf re-import");

    // HOST_VISIBLE staging buffer for the readback.
    let buf_size = u64::from(w * h * 4);
    let buf_info = vk::BufferCreateInfo::default()
        .size(buf_size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { vk.device.create_buffer(&buf_info, None) }.expect("create_buffer");
    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let mt = (0..mem_props.memory_type_count)
        .find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize].property_flags.contains(
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
        })
        .expect("host-visible memory type");
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = unsafe { vk.device.allocate_memory(&alloc, None) }.expect("allocate_memory");
    unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) }.expect("bind_buffer_memory");

    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::TRANSIENT);
    let pool = unsafe { vk.device.create_command_pool(&pool_info, None) }.expect("create pool");

    yserver::kms::vk::ops::run_one_shot_op(vk, pool, |vk, cb| {
        // Imported dma-buf comes in UNDEFINED → TRANSFER_SRC.
        let to_src = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(img.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_src);
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })];
        unsafe {
            vk.device.cmd_copy_image_to_buffer(
                cb,
                img.vk_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &region,
            );
        }
        Ok(())
    })
    .expect("readback one-shot op");

    let pixel = unsafe {
        let ptr = vk
            .device
            .map_memory(memory, 0, buf_size, vk::MemoryMapFlags::empty())
            .expect("map_memory") as *const u8;
        let off = usize::try_from((y * w + x) * 4).expect("pixel offset fits usize");
        let b = u32::from(*ptr.add(off));
        let g = u32::from(*ptr.add(off + 1));
        let r = u32::from(*ptr.add(off + 2));
        vk.device.unmap_memory(memory);
        (r << 16) | (g << 8) | b
    };

    unsafe {
        vk.device.destroy_command_pool(pool, None);
        vk.device.destroy_buffer(buffer, None);
        vk.device.free_memory(memory, None);
    }
    pixel
}

fn read_dmabuf_pixel0(vk: &Arc<VkContext>, exported: &DmabufExport, w: u32, h: u32) -> u32 {
    read_dmabuf_pixel(vk, exported, w, h, 0, 0)
}

#[test]
#[ignore = "requires a Vulkan device"]
fn promotion_preserves_content_and_is_live() {
    let Some(mut h) = test_engine_harness() else {
        return;
    };

    // Create a normal (non-exportable) server-owned pixmap, fill red.
    let pix = h.create_pixmap(64, 32, 24);
    h.fill_solid(pix, 0xFF_00_00, 64, 32); // red
    h.flush_and_wait();

    // Promote it. Old image must be retired, content preserved.
    let exported = h.promote_and_export(pix).expect("promote + export");
    let vk = Arc::clone(&h.vk);
    let pixel = read_dmabuf_pixel0(&vk, &exported, 64, 32);
    assert_eq!(pixel, 0xFF_00_00, "promoted image lost original content");

    // Liveness: a fill AFTER promotion lands in the exported backing.
    h.fill_solid(pix, 0x00_FF_00, 64, 32); // green
    h.flush_and_wait();
    let pixel2 = read_dmabuf_pixel0(&vk, &exported, 64, 32);
    assert_eq!(
        pixel2, 0x00_FF_00,
        "post-promotion write not visible in dmabuf — not live"
    );
}

#[test]
#[ignore = "requires a Vulkan device"]
fn exported_depth24_fill_batch_reaches_bottom_rows() {
    let Some(mut h) = test_engine_harness() else {
        return;
    };

    let w = 484u16;
    let h_px = 340u16;
    let pix = h.create_pixmap(w, h_px, 24);

    // Seed white so stale pixels are easy to spot in the exported readback.
    h.fill_solid(pix, 0xFF_FF_FF, w, h_px);
    h.flush_and_wait();

    let exported = h.promote_and_export(pix).expect("promote + export");
    let vk = Arc::clone(&h.vk);

    // Build a large span-style batch: thin black runs near the top and the
    // bottom of the drawable. This matches xterm's text path more closely
    // than a single full-rect fill and exercises `fill_rect_batch` with many
    // `ClearRect`s on an exported depth-24 backing.
    let mut rects = Vec::new();
    for y in [8u16, 25, 42, 59, 280, 297, 314] {
        rects.extend_from_slice(&i16::to_le_bytes(0));
        rects.extend_from_slice(&i16::to_le_bytes(i16::try_from(y).expect("y fits i16")));
        rects.extend_from_slice(&u16::to_le_bytes(w));
        rects.extend_from_slice(&u16::to_le_bytes(8));
    }
    h.backend
        .poly_fill_rectangle(None, pix, 0x00_00_00, &rects)
        .expect("poly_fill_rectangle");
    h.flush_and_wait();

    let top = read_dmabuf_pixel(&vk, &exported, u32::from(w), u32::from(h_px), 20, 12);
    let bottom = read_dmabuf_pixel(&vk, &exported, u32::from(w), u32::from(h_px), 20, 318);
    assert_eq!(top, 0x00_00_00, "top span missing from exported backing");
    assert_eq!(
        bottom, 0x00_00_00,
        "bottom span missing from exported backing while top span is present"
    );
}

#[test]
#[ignore = "requires a Vulkan device"]
fn exported_depth24_copy_area_scroll_stays_live_across_full_height() {
    let Some(mut h) = test_engine_harness() else {
        return;
    };

    let w = 160u16;
    let h_px = 170u16;
    let pix = h.create_pixmap(w, h_px, 24);

    // Seed five horizontal bands with distinct colors.
    let bands = [
        (0u16, 34u16, 0xFF_00_00),
        (34, 34, 0x00_FF_00),
        (68, 34, 0x00_00_FF),
        (102, 34, 0xFF_FF_00),
        (136, 34, 0xFF_00_FF),
    ];
    for (y, height, rgb) in bands {
        let mut rect = Vec::new();
        rect.extend_from_slice(&i16::to_le_bytes(0));
        rect.extend_from_slice(&i16::to_le_bytes(i16::try_from(y).expect("y fits i16")));
        rect.extend_from_slice(&u16::to_le_bytes(w));
        rect.extend_from_slice(&u16::to_le_bytes(height));
        h.backend
            .poly_fill_rectangle(None, pix, rgb, &rect)
            .expect("poly_fill_rectangle");
    }
    h.flush_and_wait();

    let exported = h.promote_and_export(pix).expect("promote + export");
    let vk = Arc::clone(&h.vk);

    // Scroll up by one band height and repaint the newly exposed bottom band.
    h.backend
        .copy_area(None, pix, pix, 0, 34, 0, 0, w, h_px - 34)
        .expect("copy_area");
    let mut bottom_rect = Vec::new();
    bottom_rect.extend_from_slice(&i16::to_le_bytes(0));
    bottom_rect.extend_from_slice(&i16::to_le_bytes(136));
    bottom_rect.extend_from_slice(&u16::to_le_bytes(w));
    bottom_rect.extend_from_slice(&u16::to_le_bytes(34));
    h.backend
        .poly_fill_rectangle(None, pix, 0x00_FF_FF, &bottom_rect)
        .expect("bottom repaint");
    h.flush_and_wait();

    // After the scroll:
    // - old green band moved to y=0..33
    // - old yellow band moved to y=68..101
    // - new cyan band occupies the bottom.
    assert_eq!(
        read_dmabuf_pixel(&vk, &exported, u32::from(w), u32::from(h_px), 10, 10),
        0x00_FF_00,
        "top region did not reflect CopyArea scroll"
    );
    assert_eq!(
        read_dmabuf_pixel(&vk, &exported, u32::from(w), u32::from(h_px), 10, 85),
        0xFF_FF_00,
        "middle region did not reflect CopyArea scroll"
    );
    assert_eq!(
        read_dmabuf_pixel(&vk, &exported, u32::from(w), u32::from(h_px), 10, 150),
        0x00_FF_FF,
        "bottom repaint after CopyArea did not reach exported backing"
    );
}

// ───────────────────────────────────────────────────────────────────
// Task 2.1: dma-buf EXPORT_SYNC_FILE at WRITE scope
// ───────────────────────────────────────────────────────────────────

/// On real RADV HW (which supports `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`), a
/// freshly-exported dma-buf with no outstanding GPU work must report
/// `Ready` or `Idle` — the write-scope ioctl path must actually run and
/// must not time out. It must never block or panic.
#[test]
#[ignore = "requires a Vulkan device"]
fn export_sync_file_write_scope_is_idle_on_fresh_buffer() {
    use std::os::fd::AsFd;
    use yserver::kms::vk::dri3::DmabufWait;
    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run on HW");
    let img = yserver::kms::vk::target::allocate_exportable(
        &vk,
        16,
        16,
        yserver::kms::vk::target::EXPORT_FORMAT_BGRA8,
    )
    .expect("allocate_exportable");
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).expect("export_backing");
    // No GPU work has been submitted *by the caller* via the dmabuf
    // path, so the only sane outcomes are Ready or Idle:
    // - Ready — the Vulkan allocator wrote the buffer (e.g. RADV
    //           zero-fill), the fence is already signalled; the buffer
    //           is safe to overwrite.
    // - Idle  — no fence in the reservation object (no zero-fill write).
    let r = yserver::kms::vk::dri3::wait_dmabuf_write_ready(export.fd.as_fd(), 0);
    assert_ne!(
        r,
        DmabufWait::TimedOut,
        "fresh buffer with no outstanding GPU work must not time out"
    );
    assert_ne!(
        r,
        DmabufWait::Unsupported,
        "RADV supports EXPORT_SYNC_FILE — Unsupported means the ioctl path went dead"
    );
    // Ready (RADV's zero-fill fence already signalled) or Idle (no
    // fence) are both valid.
}

// ───────────────────────────────────────────────────────────────────
// Task 1.3: dri3_export_pixmap promotes server-owned pixmaps
// ───────────────────────────────────────────────────────────────────

/// The real `dri3_export_pixmap` path must succeed on a server-owned
/// (non-imported) pixmap after the promote-if-needed gate lands.
#[test]
#[ignore = "requires a Vulkan device"]
fn dri3_export_promotes_server_owned_pixmap() {
    use std::os::fd::AsRawFd;
    use yserver_core::backend::Backend;

    let mut backend = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    // allocate_test_pixmap_bgra creates a plain server-owned pixmap
    // (no imported backing) — depth 32, BGRA8.
    let host_xid = backend
        .allocate_test_pixmap_bgra(64, 32)
        .expect("allocate_test_pixmap_bgra failed (Vk present but alloc failed?)");

    // Before this task, dri3_export_pixmap returned Err for server-owned
    // pixmaps (imported_drawable gate). After this task it must promote
    // and succeed.
    let (size, w, h, stride, depth, bpp, fd) = backend
        .dri3_export_pixmap(host_xid)
        .expect("export server-owned pixmap");

    assert_eq!(w, 64, "width mismatch");
    assert_eq!(h, 32, "height mismatch");
    assert_eq!(depth, 32, "depth mismatch");
    assert_eq!(bpp, 32, "bpp mismatch");
    assert!(
        u32::from(stride) >= 64 * 4,
        "stride {stride} too small for 64px BGRA8 row"
    );
    assert!(
        size >= u32::from(stride) * 32,
        "size {size} too small for stride {stride} * 32 rows"
    );
    assert!(fd.as_raw_fd() >= 0, "invalid fd");
}

// ───────────────────────────────────────────────────────────────────
// Task 2.2: dma-buf IMPORT_SYNC_FILE (write→read fence publication)
// ───────────────────────────────────────────────────────────────────

/// Verify that `import_dmabuf_write_fence` accepts an already-signaled
/// sync_file fd without panicking or returning an unexpected error.
///
/// Two acceptable outcomes:
/// - `Ok(())` — the kernel and driver support IMPORT_SYNC_FILE.
/// - `Err(Unsupported)` — kernel older than 5.20 / driver that rejects it.
///
/// Any other error kind is a bug.
#[test]
#[ignore = "requires a Vulkan device"]
fn import_sync_file_accepts_a_signaled_fence() {
    use std::os::fd::AsFd;

    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run on HW");

    let img = yserver::kms::vk::target::allocate_exportable(
        &vk,
        16,
        16,
        yserver::kms::vk::target::EXPORT_FORMAT_BGRA8,
    )
    .expect("allocate_exportable");

    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).expect("export_backing");

    // Produce an already-signaled sync_file by exporting a signaled
    // Vulkan semaphore.
    let sync_fd = common::signaled_sync_file(&vk);

    let r = yserver::kms::vk::dri3::import_dmabuf_write_fence(export.fd.as_fd(), sync_fd.as_fd());

    // Either accepted (Ok), or Unsupported on older kernels/drivers —
    // both are valid; anything else is an unexpected failure.
    assert!(
        r.is_ok() || matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::Unsupported),
        "import_dmabuf_write_fence returned unexpected error: {r:?}"
    );
}

// ───────────────────────────────────────────────────────────────────
// Task 2.4: exported-backing lifetime (acquire / release / FreePixmap)
// ───────────────────────────────────────────────────────────────────

/// RETENTION + RELEASE: a GLXPixmap reference keeps the exported backing
/// alive across an early `FreePixmap`; the export is torn down only once
/// the last GLX ref is released.
#[test]
#[ignore = "requires a Vulkan device"]
fn exported_backing_retained_until_glx_ref_released_then_torn_down() {
    use std::os::fd::AsFd;

    let mut backend = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let vk = backend.test_vk_arc().expect("vk arc present");

    // depth-32 BGRA8 server-owned pixmap (matches the exported format).
    let host_xid = backend
        .allocate_test_pixmap_bgra(32, 32)
        .expect("allocate_test_pixmap_bgra");

    // Simulate glXCreatePixmap acquiring a GLX ref, then the export.
    backend.acquire_glx_pixmap_export(host_xid);
    let (.., fd) = backend
        .dri3_export_pixmap(host_xid)
        .expect("dri3_export_pixmap");
    assert!(
        backend.has_export_entry(host_xid),
        "export entry should exist after export"
    );

    // RETENTION: client frees the X pixmap while the GLXPixmap still
    // references it. The entry + lifetime ref must survive, and the
    // dma-buf must still be importable via Vulkan re-import.
    backend.free_pixmap(None, host_xid).expect("free_pixmap");
    assert!(
        backend.has_export_entry(host_xid),
        "export entry must survive FreePixmap while glx_refs > 0"
    );
    assert!(
        common::dmabuf_is_importable(&vk, fd.as_fd(), 32, 32, 32),
        "backing freed while export still GLX-referenced"
    );

    // RELEASE: glXDestroyPixmap drops the last GLX ref → entry +
    // lifetime ref gone.
    backend.release_glx_pixmap_export(host_xid);
    assert!(
        !backend.has_export_entry(host_xid),
        "export entry must be torn down once glx_refs hits 0 (no leak)"
    );
}

/// A bare `BufferFromPixmap` with no GLX acquire must not leak: at
/// `FreePixmap` the export-only entry (`glx_refs == 0`) is torn down.
#[test]
#[ignore = "requires a Vulkan device"]
fn export_only_entry_is_cleaned_up_at_free_pixmap() {
    let mut backend = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let host_xid = backend
        .allocate_test_pixmap_bgra(32, 32)
        .expect("allocate_test_pixmap_bgra");

    let _ = backend
        .dri3_export_pixmap(host_xid)
        .expect("dri3_export_pixmap");
    assert!(
        backend.has_export_entry(host_xid),
        "export-only entry should exist after export"
    );

    backend.free_pixmap(None, host_xid).expect("free_pixmap"); // glx_refs == 0 → immediate teardown
    assert!(
        !backend.has_export_entry(host_xid),
        "export-only entry (glx_refs == 0) must be torn down at FreePixmap"
    );
}

/// Task 2.3 flush-wiring smoke: after writing an exported drawable and
/// flushing, the bidirectional-sync publish path runs without panicking
/// and the export remains live. The real sync-ordering validation is the
/// HW gate (Task 5.1) — there's no GL consumer in-process to observe the
/// imported write fence, so this asserts the hook is reachable and the
/// export survives a write+flush, not the fence semantics themselves.
#[test]
#[ignore = "requires a Vulkan device"]
fn exported_drawable_write_then_flush_runs_sync_publish() {
    use yserver_core::backend::Backend;

    let mut backend = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let host_xid = backend
        .allocate_test_pixmap_bgra(32, 32)
        .expect("allocate_test_pixmap_bgra");

    // Acquire + export so the drawable is tracked as live-exported.
    backend.acquire_glx_pixmap_export(host_xid);
    let _ = backend
        .dri3_export_pixmap(host_xid)
        .expect("dri3_export_pixmap");
    assert!(backend.has_export_entry(host_xid));

    // Write the exported drawable (fill) — stamps touch_render_fence on
    // the exported id, which records it into the per-flush exported_writes
    // accumulator.
    let mut rect = Vec::new();
    rect.extend_from_slice(&i16::to_le_bytes(0));
    rect.extend_from_slice(&i16::to_le_bytes(0));
    rect.extend_from_slice(&u16::to_le_bytes(32));
    rect.extend_from_slice(&u16::to_le_bytes(32));
    backend
        .poly_fill_rectangle(None, host_xid, 0xFF00_FF00, &rect)
        .expect("poly_fill_rectangle");

    // Close + flush: drives the chokepoint's wait/publish for the
    // exported write. Must not panic; export stays live.
    backend
        .engine_close_open_frame_for_timeout_for_tests()
        .expect("close open frame");
    backend
        .engine_flush_submit_group_for_tests()
        .expect("flush submit group");

    assert!(
        backend.has_export_entry(host_xid),
        "export must remain live across a write + flush"
    );

    // Release to avoid a leak in the test.
    backend.release_glx_pixmap_export(host_xid);
    assert!(!backend.has_export_entry(host_xid));
}
