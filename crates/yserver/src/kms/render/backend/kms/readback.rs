use crate::kms::render::{
    backend::{
        DirectPresentFrame, KmsBackend, SCANOUT_READBACK_GRANULE, ScanoutReadOrigin,
        ScanoutReadRoute, ScanoutReadSelection, portable::dump_cursor_record_to_ppm,
    },
    platform::PlatformBackend,
    target::Src,
};
use ash::vk;
use std::io;

fn scanout_selection_phases(
    selection: ScanoutReadSelection,
) -> &'static [crate::kms::vk::scanout::BoPhase] {
    match selection {
        ScanoutReadSelection::OnScreenOnly => &[crate::kms::vk::scanout::BoPhase::OnScreen],
        ScanoutReadSelection::PermissiveDump => &[
            crate::kms::vk::scanout::BoPhase::OnScreen,
            crate::kms::vk::scanout::BoPhase::Pending,
            crate::kms::vk::scanout::BoPhase::Submitted,
            crate::kms::vk::scanout::BoPhase::Recording,
        ],
    }
}

/// Rebase a root-absolute rect into the direct source drawable's own space.
///
/// The flipped source is blitted so that its `(0, 0)` lands at
/// (`x_off`, `y_off`) of the root-covering paint target — the same mapping
/// `materialize_direct_shadow_for_unflip` uses for its fallback Copy. M2
/// eligibility pins that target to the root origin
/// (`scanout_m2_is_authoritative_root`'s `root_coverage`) and both offsets to
/// zero (`scanout_direct_eligible`), so today this is the identity; it is
/// derived from the candidate anyway so relaxing either rule cannot silently
/// shift the read.
fn direct_scanout_route_for_rect(
    backend: &KmsBackend,
    output_idx: usize,
    frame: &DirectPresentFrame,
    rect: vk::Rect2D,
) -> io::Result<ScanoutReadRoute> {
    let source_xid = frame.candidate.src_host_xid;
    let Some(drawable) = backend.store.get(frame.source_id) else {
        return Err(io::Error::other(format!(
            "output {output_idx} scans out direct source 0x{source_xid:x}, \
             which is no longer in the drawable store"
        )));
    };
    let depth = drawable.depth;
    if !matches!(depth, 24 | 32) {
        return Err(io::Error::other(format!(
            "output {output_idx} direct source 0x{source_xid:x} has depth {depth}, \
             which is not a 32-bit scanout layout"
        )));
    }
    let extent = drawable.storage.extent;
    let sx = i64::from(rect.offset.x) - i64::from(frame.candidate.x_off);
    let sy = i64::from(rect.offset.y) - i64::from(frame.candidate.y_off);
    if sx < 0
        || sy < 0
        || sx + i64::from(rect.extent.width) > i64::from(extent.width)
        || sy + i64::from(rect.extent.height) > i64::from(extent.height)
    {
        return Err(io::Error::other(format!(
            "output {output_idx} read rect {rect:?} falls outside direct source \
             0x{source_xid:x} ({}x{} at +{}+{})",
            extent.width, extent.height, frame.candidate.x_off, frame.candidate.y_off
        )));
    }
    Ok(ScanoutReadRoute::Direct {
        source_id: frame.source_id,
        source_xid,
        depth,
        source: vk::Rect2D {
            offset: vk::Offset2D {
                x: i32::try_from(sx).unwrap_or(i32::MAX),
                y: i32::try_from(sy).unwrap_or(i32::MAX),
            },
            extent: rect.extent,
        },
    })
}

pub(in crate::kms::render::backend) fn select_scanout_read_route(
    backend: &KmsBackend,
    rect: vk::Rect2D,
    selection: ScanoutReadSelection,
) -> io::Result<ScanoutReadRoute> {
    let rx0 = i64::from(rect.offset.x);
    let ry0 = i64::from(rect.offset.y);
    let rx1 = rx0 + i64::from(rect.extent.width);
    let ry1 = ry0 + i64::from(rect.extent.height);
    let phases = scanout_selection_phases(selection);

    for (pool_idx, layout) in backend.platform.outputs.iter().enumerate() {
        // Root reads of a transformed output come from its intermediate, in
        // root space; the dump still reads the scanout, in mode space.
        let transformed = selection == ScanoutReadSelection::OnScreenOnly
            && backend.platform.output_transform(pool_idx).is_some();
        let (lx, ly, lw, lh) = if transformed {
            backend.platform.output_root_rect(pool_idx)
        } else {
            (
                layout.x,
                layout.y,
                u32::from(layout.width),
                u32::from(layout.height),
            )
        };
        let lx0 = i64::from(lx);
        let ly0 = i64::from(ly);
        let lx1 = lx0 + i64::from(lw);
        let ly1 = ly0 + i64::from(lh);
        if rx0 < lx0 || ry0 < ly0 || rx1 > lx1 || ry1 > ly1 {
            continue;
        }
        // A directly-flipped CRTC is not compositing into its pool at all, so
        // the pool BO under this rect is not what the user is looking at.
        // Never fall through to it: an unresolvable direct source is an error,
        // not a licence to return stale composed pixels.
        if let Some(frame) = backend.direct_scanout_frame_for_output(pool_idx) {
            return direct_scanout_route_for_rect(backend, pool_idx, frame, rect);
        }
        if transformed {
            return Ok(ScanoutReadRoute::Intermediate {
                output_idx: pool_idx,
                local: vk::Rect2D {
                    offset: vk::Offset2D {
                        x: i32::try_from(rx0 - lx0).unwrap_or(i32::MAX),
                        y: i32::try_from(ry0 - ly0).unwrap_or(i32::MAX),
                    },
                    extent: rect.extent,
                },
            });
        }
        let Some(pool) = backend
            .platform
            .scanout_pools
            .get(pool_idx)
            .and_then(|p| p.as_ref())
        else {
            continue;
        };
        let pool = pool.display_pool();
        for phase in phases {
            if let Some(bo_idx) = pool.bos.iter().position(|bo| bo.state.phase == *phase) {
                let local = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: i32::try_from(rx0 - lx0).unwrap_or(i32::MAX),
                        y: i32::try_from(ry0 - ly0).unwrap_or(i32::MAX),
                    },
                    extent: rect.extent,
                };
                return Ok(ScanoutReadRoute::Pool {
                    pool_idx,
                    bo_idx,
                    local,
                });
            }
        }
    }

    Err(io::Error::other(match selection {
        ScanoutReadSelection::OnScreenOnly => "root screenshot rect has no on-screen scanout bo",
        ScanoutReadSelection::PermissiveDump => "scanout rect is not covered by any pool",
    }))
}

impl KmsBackend {
    /// What is on screen under the root rect `rect`, which lies in one output.
    pub(in crate::kms::render::backend) fn read_on_screen_region(
        &mut self,
        rect: vk::Rect2D,
    ) -> io::Result<Vec<u8>> {
        read_scanout_region(self, rect, ScanoutReadSelection::OnScreenOnly)
    }
}

pub(in crate::kms::render::backend) fn read_scanout_region(
    backend: &mut KmsBackend,
    rect: vk::Rect2D,
    selection: ScanoutReadSelection,
) -> io::Result<Vec<u8>> {
    read_scanout_region_named(backend, rect, selection).map(|(bytes, _)| bytes)
}

/// Read what is actually on screen under `rect`, and say which buffer it came
/// from. Reads the directly-flipped client drawable when the covering CRTC is
/// in direct scanout, and the composited BO otherwise.
fn read_scanout_region_named(
    backend: &mut KmsBackend,
    rect: vk::Rect2D,
    selection: ScanoutReadSelection,
) -> io::Result<(Vec<u8>, ScanoutReadOrigin)> {
    if rect.extent.width == 0 || rect.extent.height == 0 {
        return Ok((Vec::new(), ScanoutReadOrigin::Empty));
    }

    let (mut source, local_rect) = match select_scanout_read_route(backend, rect, selection)? {
        ScanoutReadRoute::Pool {
            pool_idx,
            bo_idx,
            local,
        } => (ScanoutReadOrigin::ComposedPool { pool_idx, bo_idx }, local),
        ScanoutReadRoute::Intermediate { output_idx, local } => (
            ScanoutReadOrigin::TransformIntermediate { output_idx },
            local,
        ),
        ScanoutReadRoute::Direct {
            source_id,
            source_xid,
            depth,
            source,
        } => {
            // Exactly the per-drawable read `do_dump_drawables` uses for its
            // `present-src` targets, which was measured to match the screen
            // while the pool read did not. `engine.get_image` returns the same
            // BGRA8 4-byte layout the pool copy below produces.
            let bytes = backend
                .engine
                .get_image(
                    &mut backend.store,
                    &mut backend.platform.gpu,
                    Src::server_internal(source_id),
                    source,
                    depth,
                )
                .map_err(|error| {
                    io::Error::other(format!(
                        "direct scanout source 0x{source_xid:x} readback: {error:?}"
                    ))
                })?;
            let expected = usize::try_from(source.extent.width)
                .ok()
                .and_then(|w| {
                    usize::try_from(source.extent.height)
                        .ok()
                        .and_then(|h| w.checked_mul(h))
                })
                .and_then(|px| px.checked_mul(4))
                .ok_or_else(|| io::Error::other("direct scanout read size overflow"))?;
            if bytes.len() != expected {
                return Err(io::Error::other(format!(
                    "direct scanout source 0x{source_xid:x} returned {} bytes for {:?}, expected {expected}",
                    bytes.len(),
                    source,
                )));
            }
            return Ok((bytes, ScanoutReadOrigin::DirectSource { source_xid }));
        }
    };

    // A root read sees every earlier request, not the last composed frame.
    let mut fresh = None;
    if selection == ScanoutReadSelection::OnScreenOnly {
        let output_idx = match source {
            ScanoutReadOrigin::ComposedPool { pool_idx, .. } => Some(pool_idx),
            ScanoutReadOrigin::TransformIntermediate { output_idx } => Some(output_idx),
            _ => None,
        };
        if let Some(output_idx) = output_idx
            && let Some(image) = backend.fresh_root_readback(output_idx, local_rect)
        {
            source = ScanoutReadOrigin::RootReadback { output_idx };
            fresh = Some(image);
        }
    }

    let Some(vk) = backend.platform.gpu.vk.as_ref().cloned() else {
        return Err(io::Error::other("no vulkan context"));
    };
    let Some(pool_handle) = backend.platform.gpu.ops_command_pool_handle() else {
        return Err(io::Error::other("no ops command pool"));
    };

    let copy_width = local_rect.extent.width;
    let copy_height = local_rect.extent.height;
    let needed_bytes = usize::try_from(copy_width)
        .ok()
        .and_then(|w| {
            usize::try_from(copy_height)
                .ok()
                .and_then(move |h| w.checked_mul(h))
        })
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| io::Error::other("scanout copy size overflow"))?;
    let (image, copied_route) = match source {
        ScanoutReadOrigin::ComposedPool { pool_idx, bo_idx } => {
            let Some(pool) = backend
                .platform
                .scanout_pools
                .get_mut(pool_idx)
                .and_then(|p| p.as_mut())
            else {
                return Err(io::Error::other("scanout pool vanished"));
            };
            // KMS phase selection above is always against B's display pool,
            // but the composited pixels live in A's paired optimal target on
            // a copied route. Readback must therefore use that local
            // image/staging allocation with A's live Vk context; the external
            // transport is not acquired or synchronized, while display, M2
            // retention, and pageflip retirement keep using B.
            match pool {
                crate::kms::vk::scanout::OutputScanout::Shared(pool) => {
                    let Some(bo) = pool.bos.get(bo_idx) else {
                        return Err(io::Error::other("scanout bo vanished"));
                    };
                    (bo.vk_image, false)
                }
                crate::kms::vk::scanout::OutputScanout::Copied(pool) => {
                    let Some(source) = pool.sources.get(bo_idx) else {
                        return Err(io::Error::other("copied scanout source vanished"));
                    };
                    source.validate_renderer_readback()?;
                    (source.image(), true)
                }
            }
        }
        // Renderer A composes into it on either route, and leaves it GENERAL.
        ScanoutReadOrigin::TransformIntermediate { output_idx } => {
            let (image, _) = backend
                .scene
                .transform_intermediate(output_idx)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "output {output_idx} transform intermediate has not been composed"
                    ))
                })?;
            (image, false)
        }
        // Composed for this read and left `GENERAL`, like the intermediate.
        ScanoutReadOrigin::RootReadback { .. } => (fresh.expect("set with the origin"), false),
        ScanoutReadOrigin::Empty | ScanoutReadOrigin::DirectSource { .. } => {
            unreachable!("returned above")
        }
    };
    // The per-BO transfer staging is device-local write-combined memory,
    // which the CPU reads uncached; copy into the host-cached readback
    // buffer instead (x11vnc polls root rows at thousands of reads/s).
    let (staging_buffer, staging_mapped) =
        ensure_scanout_readback(&mut backend.platform, &vk, needed_bytes)?;

    let op = ensure_scanout_readback_op(&mut backend.platform, &vk, pool_handle)?;
    let run_result = op.run(|vk, cb| {
        let pre = [ash::vk::ImageMemoryBarrier2::default()
            .src_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(ash::vk::PipelineStageFlags2::COPY)
            .dst_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
            .old_layout(ash::vk::ImageLayout::GENERAL)
            .new_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(image)
            .subresource_range(
                ash::vk::ImageSubresourceRange::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let pre_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&pre);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &pre_dep) };

        let region = [ash::vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(ash::vk::Offset3D {
                x: local_rect.offset.x,
                y: local_rect.offset.y,
                z: 0,
            })
            .image_extent(ash::vk::Extent3D {
                width: copy_width,
                height: copy_height,
                depth: 1,
            })];
        unsafe {
            crate::vk_count!(cmd_copy_image_to_buffer);
            vk.device.cmd_copy_image_to_buffer(
                cb,
                image,
                ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging_buffer,
                &region,
            );
        }

        let post = [ash::vk::ImageMemoryBarrier2::default()
            .src_stage_mask(ash::vk::PipelineStageFlags2::COPY)
            .src_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
            .dst_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
            .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(ash::vk::ImageLayout::GENERAL)
            .image(image)
            .subresource_range(
                ash::vk::ImageSubresourceRange::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let post_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&post);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &post_dep) };
        Ok(())
    });

    if let Err(crate::kms::vk::ops::OneShotError {
        result: e,
        in_flight,
    }) = run_result
    {
        if in_flight {
            // The copy may still be running: abandon the readback buffer,
            // command buffer and fence rather than free what the GPU may use.
            std::mem::forget(backend.platform.scanout_readback.take());
            std::mem::forget(backend.platform.scanout_readback_op.take());
        }
        if copied_route || e == ash::vk::Result::ERROR_DEVICE_LOST {
            // Deliberately fatal on any copied-route error, even pre-submit
            // ones `in_flight` would clear: the copied source may have
            // executed work (including an ownership acquire) and is not
            // reused under an uncertain state. DEVICE_LOST is fatal on the
            // shared path too.
            backend.platform.gpu.renderer_failed = true;
        }
        return Err(io::Error::other(format!("scanout copy submit: {e:?}")));
    }

    if let Some(readback) = backend.platform.scanout_readback.as_ref() {
        readback
            .invalidate_for_read()
            .map_err(|e| io::Error::other(format!("scanout readback invalidate: {e:?}")))?;
    }
    // SAFETY: the buffer is mapped for at least `needed_bytes`, the copy's
    // fence has signalled, and `invalidate_for_read` made its writes visible.
    let raw = unsafe { std::slice::from_raw_parts(staging_mapped.as_ptr(), needed_bytes) };
    let mut bytes = raw.to_vec();
    // Reads never see a software cursor, as on Xorg (mi/misprite.c); a dump
    // shows the screen as it is.
    if selection == ScanoutReadSelection::OnScreenOnly {
        backend
            .scene
            .restore_under_cursor(image, local_rect, &mut bytes);
    }
    Ok((bytes, source))
}

/// Return the platform's reusable scanout-readback command buffer and fence,
/// recreating them for a new `VkContext` or command pool.
fn ensure_scanout_readback_op<'a>(
    platform: &'a mut PlatformBackend,
    vk: &std::sync::Arc<crate::kms::vk::device::VkContext>,
    pool: ash::vk::CommandPool,
) -> io::Result<&'a mut crate::kms::vk::ops::ReusableOneShot> {
    if !platform
        .scanout_readback_op
        .as_ref()
        .is_some_and(|op| op.matches(vk, pool))
    {
        platform.scanout_readback_op = None;
        let op = crate::kms::vk::ops::ReusableOneShot::new(std::sync::Arc::clone(vk), pool)
            .map_err(|e| io::Error::other(format!("scanout readback op alloc: {e:?}")))?;
        platform.scanout_readback_op = Some(op);
    }
    Ok(platform
        .scanout_readback_op
        .as_mut()
        .expect("scanout readback op just ensured"))
}

/// Return the platform's scanout readback buffer, (re)allocating it when it
/// is missing, smaller than `needed_bytes`, or from another `VkContext`.
fn ensure_scanout_readback(
    platform: &mut PlatformBackend,
    vk: &std::sync::Arc<crate::kms::vk::device::VkContext>,
    needed_bytes: usize,
) -> io::Result<(ash::vk::Buffer, std::ptr::NonNull<u8>)> {
    let needed = needed_bytes as u64;
    let reusable = platform
        .scanout_readback
        .as_ref()
        .is_some_and(|b| b.size() >= needed && std::sync::Arc::ptr_eq(b.vk(), vk));
    if !reusable {
        // Idle: every read waits on its own fence before returning.
        platform.scanout_readback = None;
        let size = needed
            .div_ceil(SCANOUT_READBACK_GRANULE)
            .max(1)
            .saturating_mul(SCANOUT_READBACK_GRANULE);
        let buffer = crate::kms::render::engine::StagingBuffer::new_for_readback(
            std::sync::Arc::clone(vk),
            size,
        )
        .map_err(|e| io::Error::other(format!("scanout readback alloc: {e:?}")))?;
        platform.scanout_readback = Some(buffer);
    }
    let buffer = platform
        .scanout_readback
        .as_ref()
        .expect("scanout readback just ensured");
    Ok((buffer.buffer(), buffer.mapped()))
}

pub(in crate::kms::render::backend) fn do_dump_scanout(backend: &mut KmsBackend) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};

    let mut wrote_any = false;
    let mut last_err: Option<io::Error> = None;

    static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
    let run = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);

    let layouts: Vec<(i32, i32, u16, u16)> = backend
        .platform
        .outputs
        .iter()
        .map(|layout| (layout.x, layout.y, layout.width, layout.height))
        .collect();
    for (pool_idx, (x, y, width, height)) in layouts.into_iter().enumerate() {
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let (raw, origin) =
            match read_scanout_region_named(backend, rect, ScanoutReadSelection::PermissiveDump) {
                Ok(read) => read,
                Err(err) => {
                    // Never leave the previous run's file (or nothing at all)
                    // standing in for an unreadable output: an unreadable
                    // direct source is exactly the case that used to be
                    // silently answered with a stale composed BO.
                    log::warn!("render do_dump_scanout: output {pool_idx} failed: {err}");
                    let marker = format!("./yserver-scanout-{run}-out{pool_idx}-UNREADABLE.txt");
                    if let Err(write_err) = std::fs::write(
                        &marker,
                        format!("output {pool_idx} {width}x{height} at +{x}+{y}: {err}\n"),
                    ) {
                        log::warn!("render do_dump_scanout: write {marker}: {write_err}");
                    }
                    last_err = Some(err);
                    continue;
                }
            };

        // The filename names the buffer that was actually read, so a dump
        // taken during direct scanout can never be mistaken for a composed one.
        let path = format!(
            "./yserver-scanout-{run}-out{pool_idx}-{}.ppm",
            origin.label()
        );
        use std::io::Write;
        let mut file = std::fs::File::create(&path)?;
        file.write_all(format!("P6\n{width} {height}\n255\n").as_bytes())?;
        let mut row_buf = vec![0u8; usize::from(width) * 3];
        for y in 0..usize::from(height) {
            let row_start = y * usize::from(width) * 4;
            for x in 0..usize::from(width) {
                let pi = row_start + x * 4;
                let dst = x * 3;
                row_buf[dst] = raw[pi + 2];
                row_buf[dst + 1] = raw[pi + 1];
                row_buf[dst + 2] = raw[pi];
            }
            file.write_all(&row_buf)?;
        }
        log::info!(
            "render do_dump_scanout: wrote {path} ({}x{})",
            width,
            height
        );
        wrote_any = true;
    }

    // Diagnostic: also dump the HW cursor plane's dumb buffer (kernel-side
    // view, before the display engine samples it). Compared against the
    // on-screen cursor it isolates load_image stride bugs from display-
    // engine stride misinterpretation.
    for device in &backend.platform.devices {
        if let Some(plane) = device.cursor.plane.as_ref() {
            let path = format!(
                "./yserver-cursor-{run}-{}-{}.ppm",
                device.key.major, device.key.minor
            );
            if let Err(e) = plane.dump_to_ppm(&path) {
                log::warn!(
                    "render do_dump_scanout: cursor dump for device {} failed: {e}",
                    device.key
                );
            }
        }
    }
    // Also dump the source CursorRecord bytes (BEFORE load_image), so a
    // diff between this and the dumb-buffer dump localises the bug to
    // either upstream (engine.get_image / X11 wire) or load_image itself.
    if let Some(xid) = backend.effective_cursor_xid
        && let Some(rec) = backend.cursor_records.get(&xid)
    {
        let path = format!("./yserver-cursor-src-{run}.ppm");
        if let Err(e) = dump_cursor_record_to_ppm(&path, rec) {
            log::warn!("render do_dump_scanout: cursor record dump failed: {e}");
        } else {
            log::info!(
                "render do_dump_scanout: wrote {path} (xid=0x{xid:x} \
                 {}x{} hot=({},{}) bytes_len={} version={})",
                rec.width,
                rec.height,
                rec.hot_x,
                rec.hot_y,
                rec.bgra_bytes.len(),
                rec.version,
            );
        }
    }

    if wrote_any {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| io::Error::other("scanout dump failed")))
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_readback_dump_scanout(&mut self) {
        if let Err(e) = do_dump_scanout(self) {
            log::warn!("render dump_scanout: {e}");
        }
    }
}
