use super::*;

/// Build a `PhysicalDeviceMemoryProperties` from a list of per-type
/// property-flag sets (heap indices don't matter for type selection).
fn mem_props_with(types: &[vk::MemoryPropertyFlags]) -> vk::PhysicalDeviceMemoryProperties {
    let mut mp = vk::PhysicalDeviceMemoryProperties {
        memory_type_count: types.len() as u32,
        ..Default::default()
    };
    for (i, &flags) in types.iter().enumerate() {
        mp.memory_types[i].property_flags = flags;
    }
    mp
}

#[test]
fn readback_prefers_cached_coherent_no_invalidate() {
    use vk::MemoryPropertyFlags as F;
    // DEVICE_LOCAL, write-combined coherent, then cached+coherent.
    let mp = mem_props_with(&[
        F::DEVICE_LOCAL,
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED | F::HOST_COHERENT,
    ]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, true).expect("a host type exists");
    assert_eq!(
        idx, 2,
        "must pick the cached+coherent type, not write-combined"
    );
    assert!(coherent, "cached+coherent ⇒ no manual invalidate needed");
}

#[test]
fn readback_falls_back_to_cached_noncoherent_needs_invalidate() {
    use vk::MemoryPropertyFlags as F;
    // Only write-combined coherent + cached-non-coherent on offer.
    let mp = mem_props_with(&[
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED,
    ]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, true).expect("a host type exists");
    assert_eq!(idx, 1, "cached beats write-combined for readback");
    assert!(
        !coherent,
        "cached-only ⇒ caller must invalidate before reading"
    );
}

#[test]
fn readback_falls_back_to_coherent_when_no_cached() {
    use vk::MemoryPropertyFlags as F;
    let mp = mem_props_with(&[F::DEVICE_LOCAL, F::HOST_VISIBLE | F::HOST_COHERENT]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, true).expect("a host type exists");
    assert_eq!(
        idx, 1,
        "write-combined coherent is the last-resort readback type"
    );
    assert!(coherent);
}

#[test]
fn upload_ignores_cached_and_takes_coherent() {
    use vk::MemoryPropertyFlags as F;
    // Even with a cached type present, the upload path wants COHERENT.
    let mp = mem_props_with(&[
        F::HOST_VISIBLE | F::HOST_CACHED,
        F::HOST_VISIBLE | F::HOST_COHERENT,
    ]);
    let (idx, coherent) =
        StagingBuffer::pick_memory_type(&mp, u32::MAX, false).expect("a coherent type exists");
    assert_eq!(
        idx, 1,
        "upload selects HOST_COHERENT regardless of cached availability"
    );
    assert!(coherent);
}

#[test]
fn pick_memory_type_respects_type_bits_mask() {
    use vk::MemoryPropertyFlags as F;
    // The ideal cached+coherent type (index 1) is masked out by type_bits,
    // so readback must fall through to the write-combined coherent (index 0).
    let mp = mem_props_with(&[
        F::HOST_VISIBLE | F::HOST_COHERENT,
        F::HOST_VISIBLE | F::HOST_CACHED | F::HOST_COHERENT,
    ]);
    let bits = 0b01; // only type 0 allowed
    let (idx, _) = StagingBuffer::pick_memory_type(&mp, bits, true).expect("masked selection");
    assert_eq!(
        idx, 0,
        "must honour memory_type_bits even when a better type exists"
    );
}

#[test]
fn pick_memory_type_none_when_no_host_visible() {
    use vk::MemoryPropertyFlags as F;
    let mp = mem_props_with(&[F::DEVICE_LOCAL]);
    assert!(StagingBuffer::pick_memory_type(&mp, u32::MAX, true).is_none());
    assert!(StagingBuffer::pick_memory_type(&mp, u32::MAX, false).is_none());
}

#[test]
fn close_open_frame_with_no_open_frame_returns_already_closed() {
    let mut engine = RenderEngine::stub();
    let mut store = DrawableStore::stub();
    let mut platform = PlatformBackend::for_tests();
    let out = engine
        .close_open_frame(
            &mut store,
            &mut platform,
            crate::kms::render::frame_builder::CloseReason::Shutdown,
        )
        .expect("close on a closed frame must Ok");
    assert!(matches!(
        out,
        crate::kms::render::frame_builder::CloseOutcome::AlreadyClosed
    ));
}

#[test]
fn stub_engine_declines_paint_ops() {
    let mut engine = RenderEngine::stub();
    let mut store = DrawableStore::new();
    let mut platform = PlatformBackend::for_tests();
    let storage = crate::kms::render::store::Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();
    let err = engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            [1.0, 0.0, 0.0, 1.0],
        )
        .expect_err("stub engine must reject");
    assert!(matches!(err, RenderError::NoVk));
    assert!(!engine.is_live());
}

/// Allocate a pixmap drawable in `store` backed by a real Vk
/// storage. Returns the `DrawableId`. Used by Task 3 tests.
fn create_pixmap(
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    xid: u32,
    w: u16,
    h: u16,
    depth: u8,
) -> Result<DrawableId, RenderError> {
    let storage = platform
        .allocate_drawable_storage(w, h, depth)
        .map_err(RenderError::Vk)?;
    store
        .allocate(
            xid,
            crate::kms::render::store::DrawableKind::Pixmap,
            depth,
            false,
            storage,
        )
        .map_err(|_| RenderError::NoVk)
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn engine_exposes_descriptor_pool_ring_lifetime_counters() {
    let b = match crate::kms::render::backend::KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    assert_eq!(b.engine.descriptor_pool_creates_lifetime(), 0);
    assert_eq!(b.engine.descriptor_pool_resets_lifetime(), 0);
}

// ── Task 3 Phase A regression tests ─────────────────────────

// ── Task 4 Phase A regression tests ─────────────────────────

// ── Task 7 Phase A regression tests ─────────────────────────

// ────────────────────────────────────────────────────────────
// Phase B.2 Task 4: overlay-as-source-of-truth read accessor
// + commit_close_success overlay → storage write-back.
// ────────────────────────────────────────────────────────────

/// `RenderEngineInner::current_layout_for_drawable` returns the
/// overlay's `current_in_frame_layout` once the drawable has been
/// first-touched and updated in-frame; falls back to
/// `storage.current_layout` when no frame is open / drawable
/// untouched.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn current_layout_for_drawable_reads_overlay_when_first_touched() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let id = engine
        .create_pixmap(&mut store, &mut platform, 0x4f00_0001, 8, 8, 32)
        .expect("create");

    // Storage seeded with UNDEFINED by `allocate_drawable_storage`.
    // Pre-condition (no frame open): wrapper returns the storage
    // value directly.
    {
        let inner = engine.inner.as_ref().expect("inner");
        assert_eq!(
            inner.current_layout_for_drawable(&store, id),
            vk::ImageLayout::UNDEFINED,
            "no frame open + UNDEFINED storage → wrapper returns UNDEFINED",
        );
    }

    // Open a frame and first-touch the drawable, then update its
    // in-frame layout to COLOR_ATTACHMENT_OPTIMAL — same shape a
    // ported `render_composite` will use at op-append time.
    let ticket = platform
        .submit_group_ticket_or_open()
        .expect("submit_group_ticket_or_open");
    engine.open_frame_for_paint_for_tests(ticket);
    {
        let inner = engine.inner.as_mut().expect("inner");
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.layouts
            .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
        open.layouts
            .set_drawable_in_frame(id, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    }

    // Wrapper now consults the overlay — must see the in-frame
    // value, NOT the (still UNDEFINED) storage value.
    {
        let inner = engine.inner.as_ref().expect("inner");
        assert_eq!(
            inner.current_layout_for_drawable(&store, id),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            "frame open + drawable first-touched → wrapper returns overlay's \
                 current_in_frame_layout (overlay-as-source-of-truth invariant)",
        );
    }
    // Storage unchanged during recording.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::UNDEFINED,
        "storage NOT mutated during recording (B.2 invariant)",
    );

    // Untouched second drawable in the same open frame falls
    // through to its storage layout.
    let id2 = engine
        .create_pixmap(&mut store, &mut platform, 0x4f00_0002, 8, 8, 32)
        .expect("create #2");
    {
        let inner = engine.inner.as_ref().expect("inner");
        assert_eq!(
            inner.current_layout_for_drawable(&store, id2),
            vk::ImageLayout::UNDEFINED,
            "untouched drawable in open frame → wrapper falls back to storage",
        );
    }

    // Close the frame cleanly so drop-time invariants hold.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close");
    engine.drain_all(&mut platform);
}

/// `commit_close_success` writes each touched drawable's
/// `current_in_frame_layout` back to `storage.current_layout`
/// (USER-codex U-R6.F1 — LOAD-BEARING).
///
/// Without this commit, a B.2 frame ports that route layout
/// transitions exclusively through the overlay would leave
/// `Drawable::storage.current_layout` stale after submit — the
/// next op (legacy or ported) would emit a barrier from the wrong
/// `old_layout`, corrupting / device-losing on the next render.
///
/// This unit test substitutes for the integration test sketched
/// in the plan (Step 6) which depends on Task 5's
/// `set_frame_builder_render_composite_enabled_for_tests` gate +
/// Task 8's `render_composite_via_frame_builder` body — neither
/// has landed yet. The substitute exercises the commit path
/// directly: seed the overlay manually, drive the close, assert
/// storage caught up.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn commit_close_success_writes_overlay_into_storage() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let id = engine
        .create_pixmap(&mut store, &mut platform, 0x4f01_0001, 8, 8, 32)
        .expect("create");
    // Storage starts UNDEFINED.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::UNDEFINED,
    );

    // Open a frame, seed the overlay as a ported op would: first
    // touch records the pre-frame layout, then `set_*_in_frame`
    // captures the post-op exit layout. (For `render_composite`,
    // that's SHADER_READ_ONLY_OPTIMAL per Pitfall 6.)
    let ticket = platform
        .submit_group_ticket_or_open()
        .expect("submit_group_ticket_or_open");
    engine.open_frame_for_paint_for_tests(ticket);
    {
        let inner = engine.inner.as_mut().expect("inner");
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.layouts
            .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
        open.layouts
            .set_drawable_in_frame(id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
    // While the frame is open, storage MUST NOT have moved.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::UNDEFINED,
        "storage unchanged during recording (B.2 invariant)",
    );

    // Close on success — frame has no recorded ops, so the
    // empty CB submits cleanly and `commit_close_success` runs.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close");

    // Storage MUST have caught up to the overlay's in-frame value.
    assert_eq!(
        store.get(id).expect("drawable").storage.current_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "commit_close_success wrote overlay → storage \
             (USER-codex U-R6.F1 LOAD-BEARING invariant)",
    );
    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn sampled_scratch_image_has_view_and_sampled_usage() {
    let Ok(vk) = crate::kms::vk::device::VkContext::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let vk = std::sync::Arc::new(vk);
    let s = crate::kms::render::engine::allocate_sampled_scratch_image(
        &vk,
        16,
        8,
        ash::vk::Format::B8G8R8A8_UNORM,
    )
    .expect("allocate sampled scratch");
    assert_ne!(
        s.view,
        ash::vk::ImageView::null(),
        "must expose an IDENTITY view"
    );
    assert!(s.size_bytes > 0);
}

/// #177: an upload arena block must never be handed out again while
/// the GPU can still read it. A frame's blocks stay with the frame
/// while it is open and while it is closed but its fence has not
/// signalled (recorded into the submit group, not yet submitted); a
/// frame opened meanwhile gets a block of its own. Only the retire walk
/// after the fence signals returns the blocks, and later frames then
/// reuse them without allocating.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn upload_blocks_are_not_reused_while_their_frame_is_in_flight() {
    use crate::kms::vk::mem_accounting::thread_alloc_calls;
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let target = alloc_drawable_3a(&platform, &mut store, 0x1, 32, 32);
    let pixels = [0xFFu8; 4];
    let glyphs = [CompositeGlyphInput {
        gs_xid: 0x7001,
        glyph_id: 1,
        w: 2,
        h: 2,
        pixels: GlyphPixels::A8(&pixels),
        dst_x: 1,
        dst_y: 1,
    }];
    let draw =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .composite_glyphs(
                    store,
                    platform,
                    Dst::server_internal(target),
                    3, // Over
                    0,
                    [1.0, 1.0, 1.0, 1.0],
                    &glyphs,
                    None,
                )
                .expect("composite_glyphs");
        };
    let settle =
        |engine: &mut RenderEngine, store: &mut DrawableStore, platform: &mut PlatformBackend| {
            engine
                .flush_submit_group(
                    store,
                    platform,
                    crate::kms::render::submit_group::FlushReason::SyncBoundary,
                )
                .expect("flush");
            platform.wait_idle_bounded();
            engine.poll_retired(platform);
        };
    let idle = |engine: &RenderEngine| {
        engine
            .inner
            .as_ref()
            .expect("inner")
            .upload_arena
            .idle_len()
    };
    // The upload slices of the open frame.
    let open_slices = |engine: &RenderEngine| {
        engine
            .inner
            .as_ref()
            .expect("inner")
            .frame_builder
            .open
            .as_ref()
            .expect("open frame")
            .pins
            .upload_slices
            .clone()
    };

    // Warm-up: atlas, pipelines, the glyph's upload and one block, which
    // the retire walk leaves on the idle list.
    draw(&mut engine, &mut store, &mut platform);
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close warm-up frame");
    settle(&mut engine, &mut store, &mut platform);
    assert_eq!(idle(&engine), 1);

    // Frame A: two requests share the idle block; nothing is allocated.
    let before = thread_alloc_calls();
    draw(&mut engine, &mut store, &mut platform);
    draw(&mut engine, &mut store, &mut platform);
    assert_eq!(
        thread_alloc_calls() - before,
        0,
        "frame A reuses the idle block"
    );
    let a = open_slices(&engine);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].buffer, a[1].buffer, "one block for the frame");
    assert_eq!(a[0].offset, 0);
    assert!(
        a[1].offset > 0 && a[1].offset % UPLOAD_VERTEX_ALIGN == 0,
        "{a:?}"
    );
    assert_eq!(idle(&engine), 0);

    // Closing submits frame A, whose fence may signal at any time; its
    // block only goes back on the idle list through the retire walk, so
    // don't run one until frame B has taken its block.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close frame A");
    assert_eq!(
        idle(&engine),
        0,
        "a closed frame must not return its blocks before retiring"
    );

    // Frame B, opened while A is in flight: a block of its own.
    let before = thread_alloc_calls();
    draw(&mut engine, &mut store, &mut platform);
    assert_eq!(
        thread_alloc_calls() - before,
        1,
        "frame B allocates a block"
    );
    let b = open_slices(&engine);
    assert_eq!(b.len(), 1);
    assert_ne!(
        b[0].buffer, a[0].buffer,
        "frame B was handed frame A's block while A was in flight"
    );

    // Both frames submitted and complete: both blocks go idle.
    engine
        .close_open_frame_for_timeout_for_tests(&mut store, &mut platform)
        .expect("close frame B");
    settle(&mut engine, &mut store, &mut platform);
    assert_eq!(idle(&engine), 2);

    // Frame C reuses one of them without allocating.
    let before = thread_alloc_calls();
    draw(&mut engine, &mut store, &mut platform);
    assert_eq!(thread_alloc_calls() - before, 0, "reused, not allocated");
    let c = open_slices(&engine);
    assert!(c[0].buffer == a[0].buffer || c[0].buffer == b[0].buffer);
    assert_eq!(c[0].offset, 0);
    assert_eq!(idle(&engine), 1);

    engine.drain_all(&mut platform);
}

/// #177: requests in one frame are bump-allocated from one block at
/// offsets aligned for their use, and their bytes land at those
/// offsets; a request larger than a block gets a dedicated buffer.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn upload_to_frame_suballocates_aligned_slices_and_falls_back_for_oversize() {
    use crate::kms::{
        render::upload_arena::{BLOCK_BYTES, Placement},
        vk::mem_accounting::{ChurnClass, thread_alloc_calls},
    };
    let Some(mut platform) = live_platform() else {
        eprintln!("no Vk — skipping");
        return;
    };
    let mut engine = RenderEngine::new(&platform).expect("engine");
    let ticket = platform.submit_group_ticket_or_open().expect("ticket");
    let inner = engine.inner.as_mut().expect("inner");
    inner.frame_builder.open_for_paint(ticket, 1);

    let copy_align = inner.upload_copy_align;
    assert!(
        copy_align.is_power_of_two() && copy_align >= 4,
        "{copy_align}"
    );
    assert!(copy_align <= UPLOAD_COPY_ALIGN_MAX);

    let before = thread_alloc_calls();
    let v1: Vec<u8> = (0..36).collect();
    let g: Vec<u8> = vec![0xA5; 3];
    let v2: Vec<u8> = (100..140).collect();
    let p1 = inner
        .upload_to_frame(&v1, UPLOAD_VERTEX_ALIGN, ChurnClass::GlyphRun)
        .expect("v1");
    let pg = inner
        .upload_to_frame(&g, copy_align, ChurnClass::GlyphUpload)
        .expect("g");
    let p2 = inner
        .upload_to_frame(&v2, UPLOAD_VERTEX_ALIGN, ChurnClass::Traps)
        .expect("v2");
    assert_eq!(
        thread_alloc_calls() - before,
        1,
        "three requests, one block"
    );

    let open = inner.frame_builder.open.as_ref().expect("open");
    let slice = |i: crate::kms::render::frame_builder::PinnedUploadIdx| {
        open.pins.upload_slices[i.0 as usize]
    };
    let (s1, sg, s2) = (slice(p1), slice(pg), slice(p2));
    assert_eq!(s1.buffer, sg.buffer);
    assert_eq!(s1.buffer, s2.buffer);
    assert_eq!(s1.offset, 0);
    assert_eq!(sg.offset % copy_align, 0);
    assert!(sg.offset >= 36);
    assert_eq!(s2.offset % UPLOAD_VERTEX_ALIGN, 0);
    assert!(s2.offset >= sg.offset + 3);
    assert_eq!(open.pins.uploads.shared_len(), 1);
    // The bytes are where the slices say.
    let block = open.pins.uploads.block(Placement::Shared(0));
    assert_eq!(block.buffer, s1.buffer);
    assert_eq!(block.size, BLOCK_BYTES);
    for (off, want) in [(s1.offset, &v1), (sg.offset, &g), (s2.offset, &v2)] {
        let off = usize::try_from(off).expect("offset");
        // SAFETY: the block is mapped for BLOCK_BYTES and the slice lies
        // inside it; nothing else writes it.
        let got = unsafe { std::slice::from_raw_parts(block.mapped.as_ptr().add(off), want.len()) };
        assert_eq!(got, want.as_slice());
    }

    // Oversize: a dedicated buffer at offset 0, the shared head untouched.
    let big = vec![7u8; usize::try_from(BLOCK_BYTES).expect("size") + 1];
    let before = thread_alloc_calls();
    let pb = inner
        .upload_to_frame(&big, UPLOAD_VERTEX_ALIGN, ChurnClass::Traps)
        .expect("big");
    let p3 = inner
        .upload_to_frame(&v1, UPLOAD_VERTEX_ALIGN, ChurnClass::GlyphRun)
        .expect("v3");
    assert_eq!(
        thread_alloc_calls() - before,
        1,
        "only the dedicated buffer"
    );
    let open = inner.frame_builder.open.as_ref().expect("open");
    let sb = open.pins.upload_slices[pb.0 as usize];
    let s3 = open.pins.upload_slices[p3.0 as usize];
    assert_ne!(sb.buffer, s1.buffer);
    assert_eq!(sb.offset, 0);
    assert_eq!(open.pins.uploads.dedicated_len(), 1);
    assert_eq!(s3.buffer, s1.buffer, "the shared block keeps filling");
    assert!(s3.offset > s2.offset);
    assert_eq!(open.pins.len(), 5, "one pin per request");

    engine
        .close_open_frame_for_timeout_for_tests(&mut DrawableStore::new(), &mut platform)
        .expect("close");
    engine.drain_all(&mut platform);
}
