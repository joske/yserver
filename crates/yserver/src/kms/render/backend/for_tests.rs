use super::*;

impl KmsBackend {
    /// Test-only entry point: drives the production `get_image` path
    /// but returns just the pixel bytes (header stripped). Acceptance
    /// tests use this so they can index into the result starting at
    /// pixel 0 without each one having to remember the 32-byte X11
    /// reply prefix.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn get_image_pixels_for_tests(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        use yserver_core::backend::Backend;
        let reply = self.get_image(None, host_xid, format, x, y, width, height, plane_mask)?;
        Ok(reply.map(|r| {
            assert!(
                r.len() >= 32,
                "render GetImage reply missing 32-byte header"
            );
            r[32..].to_vec()
        }))
    }

    /// #133 step 3 (P4) — test accessor: the WHOLE backing of a
    /// drawable's storage, ring included, read through the PRIVILEGED
    /// route. `get_image_pixels_for_tests` goes through the client path
    /// and is content-clipped by construction, so nothing else can
    /// observe a bordered window's ring — which is exactly the property
    /// step 3 must prove in both directions.
    ///
    /// Returns `(storage_width, storage_height, bgra_bytes)`.
    #[doc(hidden)]
    pub fn backing_pixels_for_tests(&mut self, host_xid: u32) -> Option<(u32, u32, Vec<u8>)> {
        let target = self.resolve_paint_target(host_xid)?;
        let id = target.backing_id();
        let (depth, extent) = self.store.get(id).map(|d| (d.depth, d.storage.extent))?;
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        let bytes = self
            .engine
            .get_image(
                &mut self.store,
                &mut self.platform,
                target.server_backing_src(),
                rect,
                depth,
            )
            .ok()?;
        Some((extent.width, extent.height, bytes))
    }

    /// #133 step 3 (3.3) — test accessor: a drawable's STORAGE extent,
    /// which for a window is the bordered extent `(w + 2bw) x (h + 2bw)`
    /// (Xorg `compAllocPixmap`, `composite/compalloc.c:610`).
    #[doc(hidden)]
    #[must_use]
    /// Test-only (#133): where the scene walk places every participant on
    /// output 0, as `(host xid, [(x, y, w, h)])` in output-local
    /// coordinates, plus what of it is VISIBLE. Pairs with
    /// [`Self::storage_extent_for_tests`] so a
    /// test can assert the walk never samples more than the storage
    /// holds — the wezterm white-block invariant.
    pub fn scene_participant_places_for_tests(&mut self) -> Vec<ScenePlacement> {
        crate::kms::render::scene::scene_participant_places(
            &self.core,
            &mut self.store,
            &self.windows,
            0,
            &self.platform,
        )
        .into_iter()
        .map(|(xid, place, visible)| {
            let flat = |rects: Vec<ash::vk::Rect2D>| {
                rects
                    .into_iter()
                    .map(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height))
                    .collect::<Vec<_>>()
            };
            (xid, flat(place), flat(visible))
        })
        .collect()
    }

    /// Test-only (#133): the `Visibility::On` draw list for output 0 as
    /// `(x, y, w, h)`. Pairs with
    /// [`Self::scene_participant_places_for_tests`], whose `visible` is
    /// only a BOUNDING BOX and therefore blind to a gap inside it.
    pub fn scene_draw_rects_for_tests(&mut self) -> Vec<(i32, i32, u32, u32)> {
        crate::kms::render::scene::scene_draw_rects(
            &self.core,
            &mut self.store,
            &self.windows,
            0,
            &self.platform,
        )
        .into_iter()
        .map(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height))
        .collect()
    }

    /// Test hook: MapWindow as core drives it — map, then realize every window it makes viewable.
    pub fn map_window_for_tests(&mut self, host_xid: u32) -> io::Result<()> {
        self.map_subwindow(None, host_xid)?;
        let parent_viewable = self
            .windows
            .get(&host_xid)
            .and_then(|g| g.parent)
            .is_none_or(|p| self.windows.get(&p).is_none_or(|g| g.viewable));
        if !parent_viewable {
            return Ok(());
        }
        let mut stack = vec![host_xid];
        while let Some(xid) = stack.pop() {
            self.realize_window_storage(None, xid)?;
            let mut children: Vec<(u64, u32)> = self
                .windows
                .iter()
                .filter(|(_, g)| g.parent == Some(xid) && g.mapped)
                .map(|(c, g)| (g.stack_rank, *c))
                .collect();
            children.sort_unstable();
            stack.extend(children.into_iter().rev().map(|(_, c)| c));
        }
        Ok(())
    }

    /// Test hook: UnmapWindow as core drives it — unmap, then release the subtree, child first.
    pub fn unmap_window_for_tests(&mut self, host_xid: u32) -> io::Result<()> {
        self.unmap_subwindow(None, host_xid)?;
        let mut pre = Vec::new();
        let mut stack = vec![host_xid];
        while let Some(xid) = stack.pop() {
            if !self.windows.get(&xid).is_some_and(|g| g.viewable) {
                continue;
            }
            pre.push(xid);
            stack.extend(
                self.windows
                    .iter()
                    .filter(|(_, g)| g.parent == Some(xid))
                    .map(|(c, _)| *c),
            );
        }
        for xid in pre.into_iter().rev() {
            self.release_window_storage(None, xid)?;
        }
        Ok(())
    }

    pub fn storage_extent_for_tests(&self, host_xid: u32) -> Option<(u32, u32)> {
        let id = self.store.lookup(host_xid)?;
        self.store
            .get(id)
            .map(|d| (d.storage.extent.width, d.storage.extent.height))
    }

    /// #133 step 3 (P4) — test accessor: the resolved paint target's
    /// content translation and content clip (`None` = the whole
    /// storage), plus whether the chain carries a border clip at all.
    /// The border-clip bit is the direct-scanout gate's input (3.5).
    #[doc(hidden)]
    #[must_use]
    pub fn paint_target_shape_for_tests(&self, host_xid: u32) -> Option<PaintTargetShape> {
        let t = self.resolve_paint_target(host_xid)?;
        Some((
            t.offset(),
            t.content_bounds()
                .map(|c| (c.offset.x, c.offset.y, c.extent.width, c.extent.height)),
            t.has_border_clip(),
        ))
    }

    /// #133 step 3 (P4) — test accessor for the PRIVILEGED backing
    /// route: fill a rect in BACKING coordinates, bypassing the content
    /// clip by construction. This is the shape step 4's ring fill takes
    /// (`server_backing_dst`), and the complement of the client-route
    /// clip tests: without it the suite would pass equally well on an
    /// implementation that simply cannot write the ring at all.
    #[doc(hidden)]
    pub fn fill_backing_rect_for_tests(
        &mut self,
        host_xid: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixel: u32,
    ) -> bool {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return false;
        };
        let Some(depth) = self.store.get(target.backing_id()).map(|d| d.depth) else {
            return false;
        };
        let color =
            decode_x11_pixel_for_storage(pixel, depth, PlatformBackend::format_for_depth(depth));
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x, y },
            extent: ash::vk::Extent2D { width, height },
        };
        self.engine
            .fill_rect(
                &mut self.store,
                &mut self.platform,
                target.server_backing_dst(),
                rect,
                color,
            )
            .is_ok()
    }

    /// Test accessor: the rasterised `(width, height, bgra_bytes)` of a
    /// cursor record by its host xid. Used by the acceptance harness to
    /// verify `create_cursor`'s depth-1 source/mask roundtrip through
    /// `get_image` (the #90 flattened-crosshair regression).
    #[doc(hidden)]
    #[must_use]
    pub fn cursor_record_bgra_for_tests(&self, cursor_xid: u32) -> Option<(u16, u16, Vec<u8>)> {
        self.cursor_records
            .get(&cursor_xid)
            .map(|r| (r.width, r.height, r.bgra_bytes.clone()))
    }

    /// Test fixture with live Vulkan attached. Falls back to the
    /// headless `for_tests` shape if `VkContext::new` fails. Used
    /// by the Stage 2f acceptance harness which needs real paint
    /// + readback on the v2 path.
    ///
    /// # Errors
    ///
    /// Returns `Err` only when Vk init fails AND the caller
    /// explicitly wanted Vk-backed tests; callers that can fall
    /// back to headless use `for_tests` directly.
    #[doc(hidden)]
    pub fn for_tests_with_vk() -> Result<Self, io::Error> {
        use std::sync::Arc;
        // Build the test seed WITHOUT the root drawable. If
        // `for_tests()` were used, `init_root_storage` would have
        // run with no Vk attached and stamped a `for_tests_null`
        // stub (vk::ImageView::null()) into the store. The second
        // `init_root_storage` call below would then short-circuit
        // on the existing xid and we'd be left with a null-view
        // root — any `render_composite` against it (e.g.
        // `set_container_background_pixmap`) segfaults inside the
        // descriptor-set bind.
        let mut base = Self::for_tests_seed();
        let vk = crate::kms::vk::device::VkContext::new()
            .map_err(|e| io::Error::other(format!("render for_tests_with_vk: VkContext: {e:?}")))?;
        let ops_pool = crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)).map_err(|e| {
            io::Error::other(format!("render for_tests_with_vk: OpsCommandPool: {e:?}"))
        })?;
        let fence_pool = crate::kms::render::platform::FencePool::new(Arc::clone(&vk));
        base.platform.attach_test_vk_context(vk);
        base.platform.ops_command_pool = Some(ops_pool);
        base.platform.fence_pool = Some(fence_pool);
        // Replace the stub engine with a live one now that Vk
        // is attached. Scene compositor stays stubbed (no
        // scanout pool on the test fixture).
        base.engine =
            crate::kms::render::engine::RenderEngine::new(&base.platform).map_err(|e| {
                io::Error::other(format!("render for_tests_with_vk: RenderEngine: {e:?}"))
            })?;
        base.init_root_storage();
        Ok(base)
    }

    /// Vk-backed test fixture with a live scene compositor and test
    /// scanout pools. Unlike `for_tests_with_vk`, this can drive
    /// `maybe_composite()` all the way through `scene.tick()` and an
    /// actual compose submit.
    ///
    /// # Errors
    ///
    /// Returns `Err` if Vk init, scanout-pool allocation, the render
    /// engine, or the scene compositor fails to initialise.
    #[doc(hidden)]
    pub fn for_tests_with_vk_live_scene() -> Result<Self, io::Error> {
        use std::sync::Arc;

        let mut base = Self::for_tests_seed();
        let vk = crate::kms::vk::device::VkContext::new().map_err(|e| {
            io::Error::other(format!(
                "render for_tests_with_vk_live_scene: VkContext: {e:?}"
            ))
        })?;
        let ops_pool = crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)).map_err(|e| {
            io::Error::other(format!(
                "render for_tests_with_vk_live_scene: OpsCommandPool: {e:?}"
            ))
        })?;
        let fence_pool = crate::kms::render::platform::FencePool::new(Arc::clone(&vk));
        base.platform.attach_test_vk_context(Arc::clone(&vk));
        let mut scanout_pools = Vec::with_capacity(base.platform.outputs.len());
        let mut bo_generations = Vec::with_capacity(base.platform.outputs.len());
        for (i, layout) in base.platform.outputs.iter().enumerate() {
            let kms_device = base
                .platform
                .device_for_key(layout.key.device_key)
                .expect("live-scene fixture output has a KMS owner");
            let pool = crate::kms::vk::scanout::ScanoutBoPool::allocate(
                Arc::clone(&vk),
                Rc::clone(&kms_device.device),
                layout.scanout_route,
                u32::from(layout.width),
                u32::from(layout.height),
                3,
                &layout.output.scanout_modifiers,
            )
            .map_err(|e| {
                io::Error::other(format!(
                    "render for_tests_with_vk_live_scene: ScanoutBoPool[{i}] {}x{}: {e}",
                    layout.width, layout.height
                ))
            })?;
            let n = pool.bos.len();
            scanout_pools.push(Some(crate::kms::vk::scanout::OutputScanout::Shared(pool)));
            bo_generations.push(vec![
                crate::kms::render::platform::BoGenerationEntry::default(
                );
                n
            ]);
        }
        base.platform.ops_command_pool = Some(ops_pool);
        base.platform.fence_pool = Some(fence_pool);
        base.platform.scanout_pools = scanout_pools;
        base.platform.bo_generations = bo_generations;
        base.engine =
            crate::kms::render::engine::RenderEngine::new(&base.platform).map_err(|e| {
                io::Error::other(format!(
                    "render for_tests_with_vk_live_scene: RenderEngine: {e:?}"
                ))
            })?;
        base.scene =
            crate::kms::render::scene::SceneCompositor::new(&base.platform).map_err(|e| {
                io::Error::other(format!(
                    "render for_tests_with_vk_live_scene: SceneCompositor: {e:?}"
                ))
            })?;
        base.init_root_storage();
        Ok(base)
    }

    /// Stage 4b — test-only read of the alias registry. Returns
    /// a copy of the entry if the backing xid is tracked; the
    /// `pub(crate)` `KmsCore.alias_registry` is otherwise unreachable
    /// from the `tests/` integration crate.
    #[doc(hidden)]
    #[must_use]
    pub fn test_alias_registry_get(
        &self,
        backing_xid: u32,
    ) -> Option<crate::kms::core::AliasEntry> {
        let handle = yserver_core::backend::PixmapHandle::from_raw(backing_xid)?;
        self.core.alias_registry.get(handle).copied()
    }

    /// Stage 4b — test-only read of the host_window_to_backing
    /// map. Returns the backing xid registered against
    /// `window_xid`, or `None` when the window isn't redirected.
    #[doc(hidden)]
    #[must_use]
    pub fn test_host_window_to_backing(&self, window_xid: u32) -> Option<u32> {
        self.core
            .host_window_to_backing
            .get(&window_xid)
            .map(|h| h.as_raw())
    }

    /// Stage 4c.5 — test-only probe for a drawable's presentation
    /// damage. Returns `true` iff the drawable exists, has
    /// `scene_participating=true` (the `peek_presentation_damage`
    /// gate), AND has a non-empty damage region. Used by the
    /// `automatic_redirect_backing_is_scene_participating`
    /// integration test to assert the Automatic-mode pairing
    /// actually accumulates scene damage on the backing.
    #[doc(hidden)]
    #[must_use]
    pub fn test_peek_presentation_damage_nonempty(&self, xid: u32) -> bool {
        let Some(id) = self.store.lookup(xid) else {
            return false;
        };
        self.store
            .peek_presentation_damage(id)
            .is_some_and(|snap| !snap.region.is_empty())
    }

    /// Scene-α fix — test-only read of the per-storage Vk views
    /// keyed by host xid. Returns `(image_view, sample_view)` —
    /// the attachment-side IDENTITY view and the format-aware
    /// sampling view, respectively. Used by
    /// `storage_depth24_has_distinct_sample_view` to gate the
    /// scene-side α-leak fix at the construction layer.
    #[doc(hidden)]
    #[must_use]
    pub fn test_storage_views(&self, xid: u32) -> Option<(ash::vk::ImageView, ash::vk::ImageView)> {
        let id = self.store.lookup(xid)?;
        let drawable = self.store.get(id)?;
        Some((drawable.storage.image_view, drawable.storage.sample_view))
    }

    /// GLX-TFP (Task 1.2) test shim: clone the backend's
    /// `Arc<VkContext>` so tests can drive raw Vulkan readback
    /// (re-import + staging copy) against the same device.
    #[doc(hidden)]
    pub fn test_vk_arc(&self) -> Option<std::sync::Arc<crate::kms::vk::device::VkContext>> {
        self.platform.vk.as_ref().map(std::sync::Arc::clone)
    }

    /// GLX-TFP (Task 1.2) test shim: promote a server-owned pixmap onto
    /// exportable storage and export the resulting dma-buf. Drives the
    /// production `RenderEngine::promote_drawable_exportable` +
    /// `dri3::export_backing` paths.
    #[doc(hidden)]
    pub fn promote_and_export_pixmap_for_tests(
        &mut self,
        xid: u32,
    ) -> io::Result<crate::kms::vk::dri3::DmabufExport> {
        let id = self
            .store
            .lookup(xid)
            .ok_or_else(|| io::Error::other(format!("promote: unknown xid {xid:#x}")))?;
        self.engine
            .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
            .map_err(|e| io::Error::other(format!("promote_drawable_exportable: {e:?}")))?;
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or_else(|| io::Error::other("promote: no VkContext"))?;
        let (memory, stride, size, modifier) = {
            let d = self
                .store
                .get(id)
                .ok_or_else(|| io::Error::other("promote: drawable vanished"))?;
            (
                d.storage.memory,
                d.storage.export_stride,
                d.storage.export_size,
                d.storage.export_modifier,
            )
        };
        // Export the promoted memory directly (the Storage now owns the
        // exportable image's handles; we don't have the ExportableImage
        // wrapper any more, so build the export from the raw memory handle
        // + the stride/size/modifier adopt_exportable stored).
        crate::kms::vk::dri3::export_promoted(vk, memory, stride, size, modifier)
            .map_err(|e| io::Error::other(format!("export_promoted: {e:?}")))
    }

    /// Test-only read of whether the Composite Overlay Window is
    /// Stage 4a — test-only knob to install a COMPOSITE redirect
    /// route directly via the store, bypassing 4b's protocol
    /// surface (`allocate_redirected_backing` / `name_window_pixmap`
    /// still stubs returning Err until 4b lands). The
    /// `acceptance` integration tests use this to set up
    /// routing for `resolve_paint_target` coverage without
    /// touching alias-registry / host_window_to_backing
    /// bookkeeping.
    ///
    /// `window_xid` must resolve in the store; `backing_xid` must
    /// also exist and is what window-keyed paint routes into.
    /// Returns `true` if both were resolved and the route was
    /// recorded; `false` otherwise. No side effects on damage,
    /// refcount, or `scene_participating`.
    #[doc(hidden)]
    pub fn test_set_redirected_target(&mut self, window_xid: u32, backing_xid: u32) -> bool {
        let Some(w_id) = self.store.lookup(window_xid) else {
            return false;
        };
        let Some(b_id) = self.store.lookup(backing_xid) else {
            return false;
        };
        self.store.set_redirected_target(w_id, Some(b_id));
        true
    }

    /// Headless test seed. Single 800×600 stub output; no
    /// Vulkan; no real DRM device. Mirrors `KmsBackend::for_tests`
    /// in shape so unit tests that drive v2 through
    /// `process_request` get a stable fixture.
    #[doc(hidden)]
    #[must_use]
    pub fn for_tests() -> Self {
        let mut b = Self::for_tests_seed();
        b.init_root_storage();
        // Stage 5 Phase A: seed the default-cursor record so test
        // paths that exercise `define_cursor` / effective-cursor
        // resolution have a fallback to walk to. Production uses
        // the same `init_cursor_sprite` body; the test fixture has
        // no Vk, so the sprite Pixmap allocation skips cleanly.
        let _ = b.init_cursor_sprite();
        b
    }

    /// Construct the test fixture **without** initialising root
    /// storage. Used by `for_tests_with_vk` so root allocation
    /// happens after the Vk context is attached.
    fn for_tests_seed() -> Self {
        let mut backend = Self {
            core: KmsCore::for_tests(),
            platform: PlatformBackend::for_tests(),
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine: RenderEngine::stub(),
            scene: SceneCompositor::stub(),
            windows: TrackedWindows::default(),
            subwindow_clip_cache: std::cell::RefCell::new(None),
            shape_generation: 0,
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            last_observed_pool_creates: 0,
            last_observed_pool_resets: 0,
            cow_id: None,
            deferred_cow_release: false,
            scanout_m0: ScanoutM0Telemetry::default(),
            scanout_m1: ScanoutM1ProbeCache::new(),
            scanout_m2: ScanoutM2State::new(),
            armed_vblank_targets: std::collections::HashMap::new(),
            absolute_vblank_targets: std::collections::HashMap::new(),
            crtc_queue_sequence_unsupported_devices: HashSet::new(),
            clip_mask_cache: None,
            depth1_mask_cache: crate::kms::backend::Depth1MaskCache::new(256),
            uniform_glyph_source_cache: crate::kms::backend::UniformGlyphSourceCache::new(64),
            clip_mask_snapshot: None,
            fill_pattern_cache: None,
            kms_outputs_active: true,
            clear_window_area_calls: 0,
            engine_copy_area_calls: 0,
            recent_present_pixmaps: std::collections::VecDeque::with_capacity(32),
            picture_drawable_ids: HashMap::new(),
            pending_picture_drawable_refs: HashMap::new(),
            root_readback_warn: WarnThrottle::default(),
            dst_fanout_active: false,
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
            dri3_syncobjs: HashMap::new(),
            syncobj_eventfd_supported: None,
            dmabuf_sync_file_warned: Cell::new(false),
            pending_present_batches: std::collections::VecDeque::new(),
            retained_present_wakes: std::collections::HashMap::new(),
            pending_present_source_waits: HashMap::new(),
            next_present_source_wait_id: 1,
            present_source_pins: HashMap::new(),
            next_present_source_pin_id: 1,
            pending_completed_events_on_shutdown: Vec::new(),
            cursor_records: HashMap::new(),
            cursor_pixmaps: HashMap::new(),
            next_cursor_version: 1,
            default_cursor_xid: None,
            input_only_pointer_hosts: HashMap::new(),
            effective_cursor_xid: None,
            grab_cursor_override: None,
            cursor_hidden: false,
            displayed_cursor_pending: None,
            anim_cursor_records: HashMap::new(),
            released_cursors: HashSet::new(),
            active_cursor_anim: None,
            last_drained_fb_opens: 0,
            // Test fixtures always run in Direct mode.
            vt_state: crate::vt::state::VtState::Active,
            vt_pending: crate::vt::state::VtPending::default(),
            console_guard: None,
            vt_switching_armed: false,
            led_relay: None,
            leds_sent: 0,
            floating_keyboard_states: HashMap::new(),
            lock_filter_priv_by_device: HashMap::new(),
            input_sender: None,
            crtc_config_probe_executor: None,
            pending_crtc_config_probes: HashMap::new(),
            ready_crtc_config_results: HashMap::new(),
            invalidated_crtc_config_probes: HashSet::new(),
            ready_crtc_config_announcements: VecDeque::new(),
            next_crtc_config_token: 1,
            next_device_config_token: 1,
            crtc_config_topology_epoch: 0,
            #[cfg(test)]
            crtc_config_discovery_override: None,
            input_thread_control: None,
            randr_id_alloc: RandrIdAllocator::default(),
            provider_output_sources: HashMap::new(),
            output_identity_by_id: std::collections::HashMap::new(),
            output_key_by_id: std::collections::HashMap::new(),
            crtc_key_by_id: std::collections::HashMap::new(),
            present_crtc_clock_epochs: HashMap::new(),
            next_present_crtc_clock_epoch: 1,
            hotplug_rescan_deadline: None,
            gamma_luts: RefCell::new(HashMap::new()),
            exported_dmabufs: HashMap::new(),
            dmabuf_export_supported: false,
            export_holders: Default::default(),
        };
        let live: Vec<_> = backend
            .platform
            .outputs
            .iter()
            .map(|layout| {
                (
                    layout.key.clone(),
                    ConnectorConfig::Enabled {
                        mode_w: layout.width,
                        mode_h: layout.height,
                        vrefresh: layout.output.picked.vrefresh,
                        x: layout.x,
                        y: layout.y,
                    },
                    layout.output.modes.clone(),
                    layout.output.edid.clone(),
                    layout.output.mm_width,
                    layout.output.mm_height,
                    layout.output.connector_type.clone(),
                )
            })
            .collect();
        for (key, config, modes, edid, mm_width, mm_height, connector_type) in live {
            let entry = backend.randr_id_alloc.entry_mut(&key);
            entry.connected = true;
            entry.config = config;
            entry.modes = modes;
            entry.edid = edid;
            entry.mm_width = mm_width;
            entry.mm_height = mm_height;
            entry.connector_type = connector_type;
        }
        backend
    }

    /// Phase B.2 Task 14 test helper: drive
    /// `drain_frame_builder_telemetry` from a test so the queued
    /// `FrameCloseEvent`s feed into lifetime counters without
    /// requiring a full main-loop tick. Mirrors the role of
    /// `telemetry_submit_group_flushes_for_tests` for the
    /// frame-close-event side of the telemetry pipeline.
    #[doc(hidden)]
    pub fn drain_frame_builder_telemetry_for_tests(&mut self) {
        self.drain_frame_builder_telemetry();
    }

    /// Stage 5 Task 4 layer 1: test-side retirement driver. In
    /// production, retirement runs from `on_page_flip_ready` and
    /// invokes `engine.poll_retired` + `store.poll_pending_retire`.
    /// Pixmap-only test fixtures never drive a page flip, so the
    /// ring's recycle path can't run without this hook. The body
    /// mirrors the production sequence 1:1 so the acceptance harness
    /// exercises the same code paths any future store-retirement
    /// work would touch — and adds the telemetry sync call so ring
    /// delta counters land in `self.telemetry`.
    #[doc(hidden)]
    pub fn for_tests_poll_retired(&mut self) {
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        self.sync_descriptor_pool_telemetry();
    }

    /// Stage 5 Task 6.1 — test-only: number of entries currently in
    /// the deferred PRESENT completion queue.
    #[doc(hidden)]
    pub fn pending_present_events_len_for_tests(&self) -> usize {
        self.pending_present_batches
            .iter()
            .map(|batch| batch.events.len())
            .sum()
    }

    /// Stage 5 Task 6.1 — test-only: drain a single signal-check +
    /// emit cycle without going through the main loop. Equivalent to
    /// one outer-loop iteration's drain hook.
    #[doc(hidden)]
    pub fn drain_completed_present_events_for_tests(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        self.drain_completed_present_events_impl()
    }

    /// Stage 5 Task 6.1 — test-only: flip the platform's
    /// `renderer_failed` flag. Used by the force-fire-all integration
    /// test.
    #[doc(hidden)]
    pub fn set_renderer_failed_for_tests(&mut self, v: bool) {
        self.platform.renderer_failed = v;
    }

    /// Phase A T6: flush the engine's SubmitGroup with a
    /// `SyncBoundary` reason. Convenience wrapper for acceptance
    /// tests that need to drain setup CBs between assertions.
    pub fn engine_flush_submit_group_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::submit_group::FlushReason::SyncBoundary,
            )
            .map(|_| ())
    }

    /// Phase A T6: number of ops parked in the engine's
    /// `pending_group_ops` (not yet committed to `submitted`).
    /// Exposed for acceptance regression tests.
    pub fn engine_pending_group_ops_count_for_tests(&self) -> usize {
        self.engine.pending_group_ops_count_for_tests()
    }

    /// Phase B.3 (N8): scratch vec length of the most recently submitted op
    /// in the engine. Used by `b3_close_path_scratch_walk_*` acceptance
    /// integration tests to verify the close-path walk threads the
    /// `frame_scratches` local into `SubmittedOp::scratch`.
    pub fn engine_most_recent_submitted_op_scratch_len_for_tests(&self) -> usize {
        self.engine.most_recent_submitted_op_scratch_len_for_tests()
    }

    /// Task 11: create a clip snapshot via the engine and return its opaque
    /// id as a `u64` (the integration crate can't see `SnapshotId`, which is
    /// `pub(crate)`).
    pub fn engine_create_clip_snapshot_for_tests(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<u64, io::Error> {
        self.engine
            .create_clip_snapshot(width, height)
            .map(|id| id.0)
            .map_err(|e| io::Error::other(format!("create_clip_snapshot: {e:?}")))
    }

    /// Task 11: extent (width, height) of a clip snapshot, or `None` if the id
    /// is not present in the registry.
    pub fn engine_clip_snapshot_extent_for_tests(&self, id: u64) -> Option<(u32, u32)> {
        self.engine
            .clip_snapshot_extent(crate::kms::render::engine::SnapshotId(id))
            .map(|e| (e.width, e.height))
    }

    /// Task 11: retire a clip snapshot via the engine.
    pub fn engine_retire_clip_snapshot_for_tests(&mut self, id: u64) {
        self.engine
            .retire_clip_snapshot(crate::kms::render::engine::SnapshotId(id));
    }

    /// Task 12: current_layout of a clip snapshot, or `None` if absent.
    pub fn engine_clip_snapshot_layout_for_tests(&self, id: u64) -> Option<ash::vk::ImageLayout> {
        self.engine
            .clip_snapshot_layout_for_tests(crate::kms::render::engine::SnapshotId(id))
    }

    /// Task 12: whether a clip snapshot has a `last_render_ticket`, or `None` if absent.
    pub fn engine_clip_snapshot_has_ticket_for_tests(&self, id: u64) -> Option<bool> {
        self.engine
            .clip_snapshot_has_ticket_for_tests(crate::kms::render::engine::SnapshotId(id))
    }

    /// Task 12: `snapshotted_version` of a clip snapshot, or `None` if absent.
    pub fn engine_clip_snapshot_version_for_tests(&self, id: u64) -> Option<u64> {
        self.engine
            .clip_snapshot_version(crate::kms::render::engine::SnapshotId(id))
    }

    /// Task 12: invoke `masked_copy_area` with the mask sourced from a
    /// registered clip SNAPSHOT (`snapshot_id: Some`). Drives the snapshot
    /// first-touch + terminal-state commit + close-failure rollback path.
    #[allow(clippy::too_many_arguments)]
    pub fn masked_copy_area_with_snapshot_for_tests(
        &mut self,
        src_xid: u32,
        dst_xid: u32,
        snapshot_id: u64,
        clip_origin: (i32, i32),
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        w: u16,
        h: u16,
        scissors: &[vk::Rect2D],
    ) -> io::Result<()> {
        let src = self.store.lookup(src_xid).ok_or_else(|| {
            io::Error::other(format!(
                "masked_copy_area_with_snapshot_for_tests: src xid 0x{src_xid:x} not in store"
            ))
        })?;
        let dst = self.store.lookup(dst_xid).ok_or_else(|| {
            io::Error::other(format!(
                "masked_copy_area_with_snapshot_for_tests: dst xid 0x{dst_xid:x} not in store"
            ))
        })?;
        self.engine
            .masked_copy_area_with_snapshot_for_tests(
                &mut self.store,
                &mut self.platform,
                src,
                dst,
                crate::kms::render::engine::SnapshotId(snapshot_id),
                vk::Offset2D {
                    x: i32::from(src_x),
                    y: i32::from(src_y),
                },
                vk::Offset2D {
                    x: i32::from(dst_x),
                    y: i32::from(dst_y),
                },
                vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
                [clip_origin.0, clip_origin.1],
                scissors,
            )
            .map_err(|e| {
                io::Error::other(format!(
                    "masked_copy_area_with_snapshot_for_tests: engine error: {e:?}"
                ))
            })
    }

    /// Task 13: drive `refresh_clip_snapshot` from the integration crate. Looks
    /// up the live clip-mask drawable by xid and (re)populates the snapshot to
    /// `version` (WRITE path: appends the refresh op + advances the version).
    pub fn engine_refresh_clip_snapshot_for_tests(
        &mut self,
        snapshot_id: u64,
        live_mask_xid: u32,
        version: u64,
    ) -> io::Result<()> {
        let live = self.store.lookup(live_mask_xid).ok_or_else(|| {
            io::Error::other(format!(
                "engine_refresh_clip_snapshot_for_tests: live mask xid 0x{live_mask_xid:x} not in store"
            ))
        })?;
        self.engine
            .refresh_clip_snapshot(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::engine::SnapshotId(snapshot_id),
                live,
                version,
            )
            .map_err(|e| {
                io::Error::other(format!(
                    "engine_refresh_clip_snapshot_for_tests: engine error: {e:?}"
                ))
            })
    }

    /// Phase B.2 Task 9: allocate a fresh BGRA8 pixmap via the
    /// engine's `create_pixmap`. Returns the host xid the test code
    /// uses as an opaque drawable handle; the integration crate
    /// can't see `DrawableId` (it's `pub(crate)`) so xids are the
    /// stable test surface.
    ///
    /// Returns `None` on Vk failure (e.g. test fixture without Vk
    /// or storage allocation error).
    pub fn allocate_test_pixmap_bgra(&mut self, width: u16, height: u16) -> Option<u32> {
        let xid = self.core.next_host_xid();
        let storage = self
            .platform
            .allocate_drawable_storage(width, height, 32)
            .ok()?;
        self.store_alloc(
            xid,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .ok()?;
        Some(xid)
    }

    /// Return the current `content_version` for the drawable registered under
    /// `host_xid`, or `None` if the xid is not in the store.  Integration
    /// tests use this to assert that a paint op bumped the version without
    /// having to name the private `DrawableId` type.
    pub fn drawable_content_version_for_tests(&self, host_xid: u32) -> Option<u64> {
        let id = self.store.lookup(host_xid)?;
        self.store.get(id).map(|d| d.content_version)
    }

    /// Phase B.2 Task 9: invoke `render_composite` with an empty
    /// `rects` slice. The frame-builder path's first check is
    /// `if rects.is_empty() { return Ok(stats); }` BEFORE any state
    /// mutation — used by the empty-rects-doesn't-open-frame test.
    ///
    /// `dst_xid` must resolve in the store; the function ignores the
    /// dst layout because the early-return path doesn't touch it.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store; the
    /// engine call itself is infallible on empty rects.
    pub fn render_composite_empty_for_tests(&mut self, dst_xid: u32) -> Result<(), io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(io::Error::other(format!(
                "render_composite_empty_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        const OP_SRC: u8 = 1;
        self.engine
            .render_composite(
                &mut self.store,
                &mut self.platform,
                OP_SRC,
                crate::kms::render::engine::ResolvedSource::Solid([0.0, 0.0, 0.0, 1.0]),
                crate::kms::render::engine::ResolvedSource::None,
                Dst::server_internal(dst_id),
                &[],
                None,
                crate::kms::cpu_types::Repeat::Pad,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("render_composite_empty_for_tests: {e:?}")))
    }

    /// Phase B.2 Task 11: drive a single-rect Solid-src `render_composite`
    /// against `dst_xid`. The rect covers the full dst extent (Solid
    /// fill at op=SRC). Used by the second-op-in-frame overlay test:
    /// two successive calls share the same dst, so op #2 must observe
    /// op #1's post-op layout via the overlay (Pitfall 5+6).
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails (Vk error, missing pipeline, etc.).
    pub fn render_composite_for_tests(
        &mut self,
        dst_xid: u32,
        color: [f32; 4],
        width: u32,
        height: u32,
    ) -> Result<(), io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(io::Error::other(format!(
                "render_composite_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        const OP_SRC: u8 = 1;
        let rect = crate::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width,
            height,
        };
        self.engine
            .render_composite(
                &mut self.store,
                &mut self.platform,
                OP_SRC,
                crate::kms::render::engine::ResolvedSource::Solid(color),
                crate::kms::render::engine::ResolvedSource::None,
                Dst::server_internal(dst_id),
                std::slice::from_ref(&rect),
                None,
                crate::kms::cpu_types::Repeat::Pad,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("render_composite_for_tests: {e:?}")))
    }

    /// Phase B.2 Task 17: drive `render_fill_rectangles` directly
    /// against `dst_xid`. The wrapper delegates to `render_composite`
    /// with `ResolvedSource::Solid(color)` (see
    /// `engine::render_fill_rectangles`); under sub-gate=ON this
    /// routes through the frame builder, so two calls into the same
    /// open frame collapse into a single submit.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails (Vk error, missing pipeline, etc.).
    pub fn render_fill_rectangles_for_tests(
        &mut self,
        dst_xid: u32,
        op: u8,
        color: [f32; 4],
        rects: &[crate::kms::vk::ops::render::CompositeRect],
    ) -> Result<(), io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(io::Error::other(format!(
                "render_fill_rectangles_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .render_fill_rectangles(
                &mut self.store,
                &mut self.platform,
                op,
                color,
                Dst::server_internal(dst_id),
                rects,
                None,
            )
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("render_fill_rectangles_for_tests: {e:?}")))
    }

    /// Phase B.2 Task 18: read the drawable's current Vk image layout.
    /// Used by the submit-failure rollback test to snapshot the
    /// pre-frame layout and assert that `rollback_pre_submit` restored
    /// it after the close-walk failed.
    ///
    /// Returns `vk::ImageLayout::UNDEFINED` if `dst_xid` doesn't
    /// resolve in the store (rare; production code always inserts
    /// before any layout transition).
    pub fn drawable_current_layout_for_tests(&self, dst_xid: u32) -> ash::vk::ImageLayout {
        self.store
            .get_by_xid(dst_xid)
            .map_or(ash::vk::ImageLayout::UNDEFINED, |d| {
                d.storage.current_layout
            })
    }

    /// Phase B.2 Task 11: typed peek of the open frame's recorded
    /// `RenderComposite` ops' `dst_old_layout` field, in append order.
    /// Used by the second-op-in-frame overlay test to assert that
    /// op #2 reads the overlay-resolved post-op layout of op #1
    /// (SHADER_READ_ONLY_OPTIMAL) rather than the stale storage value.
    ///
    /// The `RecordedRenderComposite` payload is `pub(crate)` so the
    /// integration test cannot match on it directly; this returns the
    /// minimum scalar needed for the assertion.
    pub fn frame_builder_peek_render_composite_dst_old_layouts_for_tests(
        &self,
    ) -> Vec<ash::vk::ImageLayout> {
        self.engine
            .frame_builder_peek_render_composite_dst_old_layouts()
    }

    /// Phase B.1 Task 15: is the frame builder currently open?
    pub fn frame_builder_is_open_for_tests(&self) -> bool {
        self.engine.frame_builder_is_open()
    }

    /// Phase B.1 Task 15: lifetime closes counter snapshot.
    pub fn frame_builder_lifetime_closes_for_tests(&self) -> u64 {
        self.engine.frame_builder_lifetime_closes()
    }

    /// Phase B.1 Task 15: drive one `maybe_composite` tick. Used by
    /// integration tests that need to trigger an M3 close without
    /// going through a full backend tick.
    pub fn tick_maybe_composite_for_tests(&mut self) {
        let _ = self.maybe_composite();
    }

    /// Phase B.1 Task 15: engine's monotonic `frame_seq` counter.
    /// Bumped by `close_open_frame` on every successful close.
    pub fn engine_frame_seq_for_tests(&self) -> u64 {
        self.engine.engine_frame_seq()
    }

    /// Phase B.2 Task 3 (Mechanism 2 watermark): set the engine's
    /// `acquire_generation` field directly. Used by the integration
    /// test to seed a known baseline before exercising the
    /// frame-open + descriptor-acquire dance, so the captured
    /// `frame_generation` assertions are deterministic.
    pub fn engine_acquire_generation_set_for_tests(&mut self, value: u64) {
        self.engine.set_acquire_generation_for_tests(value);
    }

    /// Phase B.2 Task 3: open a frame end-to-end (acquire the
    /// platform's submit-group ticket via
    /// `submit_group_ticket_or_open`, then drive the engine's
    /// `open_for_paint`). Combines the two steps so the test
    /// surface doesn't need to expose the crate-private
    /// `FenceTicket` type. The engine bumps `acquire_generation`
    /// and stamps the resulting value on the OpenFrame as
    /// `frame_generation` (Phase B.2 Mechanism 2).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the platform's fence pool or Vk context is
    /// missing (test fixture built without Vk).
    pub fn engine_open_frame_for_paint_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        let ticket = self.platform.submit_group_ticket_or_open()?;
        self.engine.open_frame_for_paint_for_tests(ticket);
        Ok(())
    }

    /// Phase B.2 Task 3: read the open frame's captured
    /// `frame_generation`. Returns `None` if no frame is open.
    pub fn engine_open_frame_generation_for_tests(&self) -> Option<u64> {
        self.engine.open_frame_generation()
    }

    /// Phase B.2 Task 3: call
    /// `RenderEngineInner::acquire_descriptor_set_for_frame_or_op`
    /// against `layout`. Used by the Mechanism 2 integration test
    /// to confirm the helper tags the active descriptor pool with
    /// the open frame's `frame_generation` (or bumps
    /// `acquire_generation` when no frame is open).
    ///
    /// # Errors
    ///
    /// Propagates `vkAllocateDescriptorSets` / `vkResetDescriptorPool`
    /// errors verbatim.
    pub fn engine_acquire_descriptor_set_for_frame_or_op_for_tests(
        &mut self,
        layout: ash::vk::DescriptorSetLayout,
    ) -> Result<ash::vk::DescriptorSet, ash::vk::Result> {
        self.engine
            .acquire_descriptor_set_for_frame_or_op_for_tests(layout)
    }

    /// Phase B.2 Task 3: build a transient
    /// `vk::DescriptorSetLayout` (single COMBINED_IMAGE_SAMPLER
    /// binding, fragment-stage) for the Mechanism 2 integration
    /// test to feed into
    /// `engine_acquire_descriptor_set_for_frame_or_op_for_tests`.
    /// The caller is responsible for calling
    /// `engine_destroy_descriptor_set_layout_for_tests` after the
    /// test finishes (the layout outlives the descriptor sets;
    /// pool reset on backend drop reclaims the sets, but the
    /// layout handle leaks if not explicitly destroyed).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the platform has no Vk context (test
    /// fixture built without Vk) or `vkCreateDescriptorSetLayout`
    /// fails.
    pub fn engine_create_test_descriptor_set_layout_for_tests(
        &self,
    ) -> Result<ash::vk::DescriptorSetLayout, ash::vk::Result> {
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let bindings = [ash::vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(ash::vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(ash::vk::ShaderStageFlags::FRAGMENT)];
        let info = ash::vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: VkContext.device is a valid device handle for the
        //         platform's lifetime; layout creation has no
        //         outstanding-handle preconditions.
        unsafe { vk.device.create_descriptor_set_layout(&info, None) }
    }

    /// Phase B.2 Task 3: destroy a layout created via
    /// `engine_create_test_descriptor_set_layout_for_tests`.
    pub fn engine_destroy_descriptor_set_layout_for_tests(
        &self,
        layout: ash::vk::DescriptorSetLayout,
    ) {
        let Some(vk) = self.platform.vk.as_ref() else {
            return;
        };
        // SAFETY: layout was created by the corresponding
        //         `create_descriptor_set_layout` helper above on
        //         this same device; no descriptor sets remain
        //         active that reference it after the ring's pool
        //         reset on backend drop.
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    /// Phase B.2 Task 3: descriptor pool ring's max
    /// `high_water_generation` across all resident pools. Returns 0
    /// before the first acquire. Used by the Mechanism 2 integration
    /// test to assert each acquire tagged the pool with the
    /// frame's captured `frame_generation`.
    pub fn descriptor_pool_ring_high_water_generation_for_tests(&self) -> u64 {
        self.engine.descriptor_pool_ring_high_water_generation()
    }

    /// Phase B.2 Task 3: unconditionally close the open frame with
    /// `CloseReason::Timeout`. The integration test uses this to
    /// transition from frame-open → frame-closed without waiting
    /// on a wall-clock timeout.
    ///
    /// # Errors
    ///
    /// Propagates the engine's close-path error (rare; renderer
    /// failure or Vk submit error).
    pub fn engine_close_open_frame_for_timeout_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        self.engine
            .close_open_frame_for_timeout_for_tests(&mut self.store, &mut self.platform)
            .map_err(|e| match e {
                crate::kms::render::engine::RenderError::Vk(r) => r,
                _ => ash::vk::Result::ERROR_UNKNOWN,
            })
    }

    /// #214: real `vkQueueSubmit2` calls made by this backend's platform
    /// for paint groups and Present signals (per instance: parallel-safe).
    pub fn platform_queue_submit_count_for_tests(&self) -> u64 {
        self.platform.queue_submit_count()
    }

    /// #214: arm a recording failure in the next frame close.
    pub fn force_next_frame_record_failure_for_tests(&mut self) {
        self.platform
            .force_next_frame_record_failure_for_integration_tests();
    }

    /// Phase A T6: size of the current SubmitGroup (number of CBs
    /// buffered and not yet submitted to the Vulkan queue).
    /// Exposed for acceptance regression tests.
    pub fn platform_submit_group_size_for_tests(&self) -> usize {
        self.platform.submit_group_size()
    }

    /// Phase A T6: true while the SubmitGroup is open (has at least
    /// one CB buffered). Exposed for acceptance regression tests.
    pub fn platform_submit_group_is_open_for_tests(&self) -> bool {
        self.platform.submit_group_is_open()
    }

    /// Phase A T8: override the SubmitGroup max-size cap so regression
    /// tests can exercise the auto-flush boundary at a specific count
    /// without depending on the production default.
    pub fn platform_submit_group_set_max_size_for_tests(&mut self, n: usize) {
        self.platform.submit_group_set_max_size_for_tests(n);
    }

    /// Phase B.1 Task 10: read back the SubmitGroup max_size as
    /// currently configured on the platform. Exposed as `pub` (not
    /// `#[cfg(test)]`) so the external `acceptance` integration-test
    /// crate can assert Invariant M1 directly.
    pub fn platform_submit_group_max_size_for_tests(&self) -> usize {
        self.platform.submit_group_max_size()
    }

    /// Phase A T10: inject a `queue_submit2` failure on the next
    /// `flush_submit_group` call. Delegates to a non-`#[cfg(test)]`
    /// method on `PlatformBackend` so this wrapper is visible from
    /// the external `acceptance` integration-test crate.
    pub fn platform_force_next_submit_failure_for_tests(&mut self) {
        self.platform
            .force_next_submit_failure_for_integration_tests();
    }

    /// Phase A T10: returns `platform.renderer_failed`. Exposed as
    /// `pub` so the `acceptance` integration test can assert the
    /// fatal-failure invariant without direct field access.
    pub fn platform_renderer_failed_for_tests(&self) -> bool {
        self.platform.renderer_failed
    }

    /// Phase A T10: count of in-flight submits awaiting retirement.
    /// Delegates to `engine.pending_count()` (`pub(crate)`). Exposed
    /// as `pub` for the `acceptance` integration test.
    pub fn engine_pending_count_for_tests(&self) -> usize {
        self.engine.pending_count()
    }

    /// Phase A T10: call `engine.fill_rect` directly and return
    /// `true` iff the result is `RenderError::RendererFailed`. Used
    /// by the failure-rollback regression test to assert that paint
    /// ops short-circuit after the renderer has been poisoned.
    pub fn engine_fill_rect_is_renderer_failed_for_tests(&mut self, host_xid: u32) -> bool {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return false;
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 1,
                height: 1,
            },
        };
        matches!(
            self.engine.fill_rect(
                &mut self.store,
                &mut self.platform,
                target.server_backing_dst(),
                rect,
                [1.0_f32, 0.0, 0.0, 1.0],
            ),
            Err(crate::kms::render::engine::RenderError::RendererFailed)
        )
    }

    /// Phase A T10: check whether `host_xid` still resolves in the
    /// drawable store after a renderer failure. Returns `Some(true)` if
    /// the drawable exists (regardless of its internal state) or `None`
    /// if the xid is unknown. Used by the rollback regression test to
    /// assert that store state is not corrupted to the point of
    /// panicking on lookup.
    pub fn store_drawable_exists_for_tests(&self, host_xid: u32) -> bool {
        self.store.get_by_xid(host_xid).is_some()
    }

    /// The `export holders` report for the current state, without change detection.
    #[doc(hidden)]
    pub fn export_holders_report_for_tests(
        &self,
        core: &yserver_core::backend::export_holders::CoreHolders,
    ) -> Vec<String> {
        let mut r = crate::kms::render::export_holders::ExportHoldersReporter::default();
        r.observe(self.export_holder_rows());
        crate::kms::render::export_holders::format_report(r.rows(), core)
    }

    /// Phase A T7: simulate the pageflip-retire frame-boundary flush
    /// without going through `on_page_flip_ready` (which calls
    /// `drain_page_flip_events` and would error on the test fixture's
    /// `/dev/null` DRM device). Replicates the full production
    /// `on_page_flip_ready` close-then-flush sequence: flush_render_batch
    /// before flush_submit_group(PageflipRetire), so regression tests
    /// exercise the real fix path.
    pub fn simulate_page_flip_complete_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            &mut self.platform,
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("simulate_page_flip_complete_for_tests: flush_render_batch failed: {e:?}");
        }
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::render::submit_group::FlushReason::PageflipRetire,
            )
            .map(|_| ())
    }

    /// Test-side page-flip completion path for live-scene fixtures.
    /// Mirrors the production `on_page_flip_ready` retire sequence
    /// without reading DRM events from the test fixture's `/dev/null`
    /// device.
    ///
    /// Returns the number of outputs whose pending scene ack retired.
    pub fn simulate_scene_page_flip_complete_for_tests(
        &mut self,
    ) -> Result<usize, ash::vk::Result> {
        self.simulate_page_flip_complete_for_tests()?;
        let mut retired = 0usize;
        for output_idx in 0..self.platform.outputs.len() {
            if self
                .scene
                .handle_page_flip_complete(output_idx, &mut self.store, &mut self.platform)
            {
                retired += 1;
            }
        }
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        Ok(retired)
    }

    /// Phase B.2 Task 12: read the global `vkQueueSubmit2` counter
    /// from `vk::call_stats`. Used as a coarse process-level submit
    /// counter when telemetry-side accounting would miss out-of-band
    /// submits (e.g. `run_one_shot_op` for asset init, which bypasses
    /// the submit-group entirely).
    ///
    /// The counter is process-global and monotonic across all tests;
    /// callers MUST capture the value at the start of their test
    /// scope and assert on the delta. For parallel-safe lifecycle
    /// counts use `telemetry_submit_group_flushes_for_tests` instead —
    /// it's per-backend and only ticks on `flush_submit_group`
    /// outcomes (which is the frame-builder collapse target).
    pub fn platform_queue_submit2_count_for_tests(&self) -> u64 {
        crate::kms::vk::call_stats::queue_submit2_count()
    }

    /// Phase A T12: drain pending flush outcomes into telemetry, then
    /// return `telemetry.lifetime.submit_group_flushes`. Using the
    /// per-backend lifetime counter instead of the global
    /// `queue_submit2_count` avoids inter-test interference when the
    /// suite runs in parallel.
    pub fn telemetry_submit_group_flushes_for_tests(&mut self) -> u64 {
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        self.telemetry.lifetime.submit_group_flushes
    }

    /// Phase B.3 Task 2 (N1, N8, N9): read the lifetime
    /// `frame_builder_close_reason_non_ported_paint_op` counter after
    /// draining pending flush outcomes. Used by the copy_area collapse
    /// integration test to assert that copy_area no longer fires the
    /// M2 close (CloseReason::NonPortedPaintOp).
    pub fn telemetry_close_reason_non_ported_for_tests(&mut self) -> u64 {
        // Drain flush outcomes into telemetry first (same drain pattern as
        // telemetry_submit_group_flushes_for_tests above).
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Close-REASON counters accumulate from frame-builder close
        // EVENTS, not flush outcomes — without this drain the counter
        // stays 0 no matter how many closes fired in the engine.
        self.drain_frame_builder_telemetry();
        self.telemetry
            .lifetime
            .frame_builder_close_reason_non_ported_paint_op
    }

    /// #137 step 5: read the lifetime
    /// `frame_builder_close_reason_sync_wait` counter after draining
    /// pending flush outcomes and frame-close events. Same drain shape
    /// as [`Self::telemetry_close_reason_non_ported_for_tests`].
    ///
    /// This is the oracle for the whole justification of the
    /// uniform-glyph-source cache: the readback's value is not the copy
    /// it avoids but the `CloseReason::SyncWait` frame close it avoids,
    /// so a cache hit must leave this counter unmoved.
    pub fn telemetry_close_reason_sync_wait_for_tests(&mut self) -> u64 {
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Close-REASON counters accumulate from frame-builder close
        // EVENTS, not flush outcomes — without this drain the counter
        // stays 0 no matter how many closes fired in the engine.
        self.drain_frame_builder_telemetry();
        self.telemetry.lifetime.frame_builder_close_reason_sync_wait
    }

    /// Phase B.3 Task 2 (N1, N8, N9): drive `engine.copy_area` directly
    /// against the store + platform using DrawableIds resolved from host
    /// xids. Mirror of `render_composite_for_tests`. Used by the copy_area
    /// collapse integration test.
    ///
    /// # Errors
    ///
    /// Returns `Err` if either xid doesn't resolve in the store, or if
    /// the engine call fails.
    pub fn engine_copy_area_for_tests(
        &mut self,
        src_xid: u32,
        dst_xid: u32,
        src_rect: ash::vk::Rect2D,
        dst_pos: ash::vk::Offset2D,
    ) -> Result<(), std::io::Error> {
        let Some(src_id) = self.store.lookup(src_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_copy_area_for_tests: src xid 0x{src_xid:x} not in store"
            )));
        };
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_copy_area_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .copy_area(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(src_id),
                Dst::server_internal(dst_id),
                src_rect,
                dst_pos,
            )
            .map_err(|e| std::io::Error::other(format!("engine_copy_area_for_tests: {e:?}")))
    }

    /// B.3 Task 4 — test-only: call `engine.cow_copy_area` against the
    /// registered `cow_id`, using the given src xid, src_rect, and
    /// dst_pos. Returns Err if no cow_id is registered or if the engine
    /// call fails.
    pub fn engine_cow_copy_area_for_tests(
        &mut self,
        src_xid: u32,
        src_rect: ash::vk::Rect2D,
        dst_pos: ash::vk::Offset2D,
    ) -> Result<(), std::io::Error> {
        let cow_id = self.cow_id.ok_or_else(|| {
            std::io::Error::other(
                "engine_cow_copy_area_for_tests: no cow_id registered; \
                 call get_overlay_window first",
            )
        })?;
        let src_id = self.store.lookup(src_xid).ok_or_else(|| {
            std::io::Error::other(format!(
                "engine_cow_copy_area_for_tests: src xid 0x{src_xid:x} not in store"
            ))
        })?;
        self.engine
            .cow_copy_area(
                &mut self.store,
                &mut self.platform,
                Dst::server_internal(cow_id),
                Src::server_internal(src_id),
                src_rect,
                dst_pos,
            )
            .map_err(|e| std::io::Error::other(format!("engine_cow_copy_area_for_tests: {e:?}")))
    }

    /// B.3 Task 6 — test-only: invoke `engine.put_image` against the given
    /// dst_xid. Constructs a staging payload of `src_extent.width *
    /// src_extent.height * 4` bytes (BGRA, depth 32) from `pixel_bytes`.
    /// Returns `Err` if `dst_xid` doesn't resolve or if the engine call fails.
    #[allow(
        dead_code,
        reason = "used by frame_builder_put_image_collapses_two_in_one_frame"
    )]
    pub fn engine_put_image_for_tests(
        &mut self,
        dst_xid: u32,
        dst_pos: ash::vk::Offset2D,
        src_extent: ash::vk::Extent2D,
        pixel_bytes: &[u8],
        src_depth: u8,
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_put_image_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .put_image(
                &mut self.store,
                &mut self.platform,
                Dst::server_internal(dst_id),
                dst_pos,
                src_extent,
                pixel_bytes,
                src_depth,
            )
            .map_err(|e| std::io::Error::other(format!("engine_put_image_for_tests: {e:?}")))
    }

    /// B.3 Task 8 — test-only: invoke `engine.fill_rect_batch` against
    /// the given `dst_xid` with `color` and `rects`. Returns `Err` if
    /// `dst_xid` doesn't resolve in the store or if the engine call fails.
    #[allow(
        dead_code,
        reason = "used by frame_builder_fill_rect_batch_collapses_two_in_one_frame"
    )]
    pub fn engine_fill_rect_batch_for_tests(
        &mut self,
        dst_xid: u32,
        color: [f32; 4],
        rects: &[ash::vk::Rect2D],
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_fill_rect_batch_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .fill_rect_batch(
                &mut self.store,
                &mut self.platform,
                Dst::server_internal(dst_id),
                color,
                rects,
            )
            .map_err(|e| std::io::Error::other(format!("engine_fill_rect_batch_for_tests: {e:?}")))
    }

    /// B.3 Task 10 — test-only: invoke `engine.logic_fill` against the
    /// given `dst_xid`. Converts the `dst_xid` to a `DrawableId` via
    /// the store lookup.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store or if
    /// the engine call fails.
    #[allow(
        dead_code,
        reason = "used by frame_builder_logic_fill_collapses_two_in_one_frame"
    )]
    pub fn engine_logic_fill_for_tests(
        &mut self,
        dst_xid: u32,
        function: yserver_core::backend::GcFunction,
        opaque_alpha: bool,
        fg: u32,
        rects: &[crate::kms::cpu_types::Rectangle16],
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_logic_fill_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .logic_fill(
                &mut self.store,
                &mut self.platform,
                Dst::server_internal(dst_id),
                function,
                opaque_alpha,
                fg,
                rects,
            )
            .map_err(|e| std::io::Error::other(format!("engine_logic_fill_for_tests: {e:?}")))
    }

    /// Phase B.3 Task 14 (N7): drive `engine.image_text` directly against the
    /// store + platform using a DrawableId resolved from a host xid.
    /// Constructs one non-zero glyph per entry in `glyphs`: each glyph is
    /// `w × h` pixels of 0xFF alpha. `font_xid` keys the glyph atlas.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails.
    #[allow(
        dead_code,
        reason = "used by frame_builder_image_text_* integration tests"
    )]
    pub fn engine_image_text_for_tests(
        &mut self,
        dst_xid: u32,
        font_xid: u32,
        foreground_rgba: [f32; 4],
        glyphs: &[(u32, i32, i32, u32, u32)], // (codepoint, dst_x, dst_y, w, h)
    ) -> Result<(u32, u32, u32), std::io::Error> {
        // Returns (atlas_interns, glyph_uploads, glyphs_dropped).
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_image_text_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        let prepared: Vec<crate::kms::render::engine::PreparedGlyph> = glyphs
            .iter()
            .map(|&(codepoint, dst_x, dst_y, w, h)| {
                let w_us = w as usize;
                let h_us = h as usize;
                let pixels = vec![0xFFu8; w_us * h_us];
                crate::kms::render::engine::PreparedGlyph {
                    codepoint,
                    dst_x,
                    dst_y,
                    w: w_us,
                    h: h_us,
                    pixels,
                }
            })
            .collect();
        self.engine
            .image_text(
                &mut self.store,
                &mut self.platform,
                Dst::server_internal(dst_id),
                font_xid,
                foreground_rgba,
                &prepared,
            )
            .map(|s| (s.atlas_interns, s.glyph_uploads, s.glyphs_dropped))
            .map_err(|e| std::io::Error::other(format!("engine_image_text_for_tests: {e:?}")))
    }

    /// Phase B.3 Task 14 (N7, N10): attach a synthetic PRESENT completion
    /// to the open frame if the given `dst_xid` is written by any op in
    /// the open frame. Mirrors `attach_synthetic_present_completion_to_cow_for_tests`
    /// but works for any drawable (image_text dst, not just the COW).
    ///
    /// Returns `true` if attach succeeded, `false` if no open frame exists
    /// or the drawable is not written in the open frame.
    #[allow(
        dead_code,
        reason = "used by frame_builder_image_text_delivers_present_completion"
    )]
    pub fn attach_synthetic_present_completion_for_tests(
        &mut self,
        dst_xid: u32,
        synthetic_serial: u32,
    ) -> bool {
        use crate::kms::render::present_completion::{PendingPresentEntry, PinnedWake};
        use yserver_core::backend::{CompletedPresentEvent, PresentWake};
        use yserver_protocol::x11::ClientId;

        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return false;
        };
        let entry = PendingPresentEntry {
            wake_pin: PinnedWake::None,
            event: CompletedPresentEvent {
                client_id: ClientId(0),
                serial: synthetic_serial,
                host_xid: 0,
                dst_host_xid: 0,
                options: 0,
                present_id: 0,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
        };
        self.engine.attach_present_completion(dst_id, entry).is_ok()
    }

    /// Phase B.3 Task 12 (N5): drive `engine.render_traps_or_tris` directly
    /// against the store + platform using a DrawableId resolved from a host xid.
    /// Uses a single-trapezoid solid-src op (PictOp 1 = Src, one trapezoid
    /// instance, small bbox). Mirror of `render_composite_for_tests`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails.
    pub fn engine_render_traps_or_tris_for_tests(
        &mut self,
        dst_xid: u32,
        color: [f32; 4],
        bbox_w: u32,
        bbox_h: u32,
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_render_traps_or_tris_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        // Pack one minimal trapezoid instance: 4× f32 pairs (top, bottom, left,
        // right edges) = 32 bytes. The actual shape doesn't matter for the
        // frame-builder collapse test — we just need a non-empty, non-zero bbox.
        let instance_data = [0u8; 32];
        self.engine
            .render_traps_or_tris(
                &mut self.store,
                &mut self.platform,
                1, // PictOp_Src
                crate::kms::render::engine::ResolvedSource::Solid(color),
                Dst::server_internal(dst_id),
                crate::kms::render::engine::TrapPrimKind::Trapezoid,
                &instance_data,
                1,
                (0, 0, bbox_w, bbox_h),
                None,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                0, // src_origin_x
                0, // src_origin_y
                0, // src_pict_format
                0, // dst_pict_format
            )
            .map(|_| ())
            .map_err(|e| {
                std::io::Error::other(format!("engine_render_traps_or_tris_for_tests: {e:?}"))
            })
    }

    /// B.3 Task 12 hotfix 2 — test-only: build a linear gradient LUT
    /// and stash it under `grad_xid` in the engine's `picture_paint`
    /// map. Uses a two-stop (black → white) 1-pixel gradient. Returns
    /// `Err` on `NoVk` or GPU allocation failure.
    pub fn engine_build_linear_gradient_for_tests(
        &mut self,
        grad_xid: u32,
    ) -> Result<(), std::io::Error> {
        use crate::kms::vk::gradient::Stop;
        self.engine
            .build_and_insert_linear_gradient(
                &mut self.platform,
                grad_xid,
                (0, 0),
                (64 << 16, 0),
                &[
                    Stop {
                        pos: 0,
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0xFFFF,
                    },
                    Stop {
                        pos: 0x10000,
                        r: 0xFFFF,
                        g: 0xFFFF,
                        b: 0xFFFF,
                        a: 0xFFFF,
                    },
                ],
            )
            .map_err(|e| {
                std::io::Error::other(format!("engine_build_linear_gradient_for_tests: {e:?}"))
            })
    }

    /// B.3 Task 12 hotfix 2 — test-only: remove a gradient from the
    /// engine's `picture_paint` map (mirrors `render_free_picture`'s
    /// inner call to `picture_paint_remove`).
    pub fn engine_picture_paint_remove_for_tests(&mut self, grad_xid: u32) {
        self.engine.picture_paint_remove(grad_xid);
    }

    /// B.3 Task 12 hotfix 2 — test-only: drive
    /// `engine.render_traps_or_tris` with a `ResolvedSource::Gradient`
    /// src for the given `grad_xid`. Uses the same single-trapezoid
    /// geometry as `engine_render_traps_or_tris_for_tests`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve, the gradient is not
    /// in `picture_paint`, or the engine call fails.
    pub fn engine_render_traps_or_tris_gradient_for_tests(
        &mut self,
        dst_xid: u32,
        grad_xid: u32,
        bbox_w: u32,
        bbox_h: u32,
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_render_traps_or_tris_gradient_for_tests: \
                 dst xid 0x{dst_xid:x} not in store"
            )));
        };
        let instance_data = [0u8; 32];
        self.engine
            .render_traps_or_tris(
                &mut self.store,
                &mut self.platform,
                1, // PictOp_Src
                crate::kms::render::engine::ResolvedSource::Gradient(grad_xid),
                Dst::server_internal(dst_id),
                crate::kms::render::engine::TrapPrimKind::Trapezoid,
                &instance_data,
                1,
                (0, 0, bbox_w, bbox_h),
                None,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                0, // src_origin_x
                0, // src_origin_y
                0, // src_pict_format
                0, // dst_pict_format
            )
            .map(|_| ())
            .map_err(|e| {
                std::io::Error::other(format!(
                    "engine_render_traps_or_tris_gradient_for_tests: {e:?}"
                ))
            })
    }

    /// Phase B.3 Task 12 (N5): read the lifetime
    /// `frame_builder_close_reason_scratch_grow` counter after draining
    /// pending flush outcomes. Used by the cross-frame mask-grow integration
    /// test to assert that close-before-grow fires exactly once during the
    /// 3-op (small, large, large) sequence.
    pub fn telemetry_close_reason_scratch_grow_for_tests(&mut self) -> u64 {
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Close-REASON counters accumulate from frame-builder close
        // EVENTS, not flush outcomes — without this drain the counter
        // stays 0 no matter how many ScratchGrow closes fired (the
        // cross-frame mask-grow test was born failing because of it).
        self.drain_frame_builder_telemetry();
        self.telemetry
            .lifetime
            .frame_builder_close_reason_scratch_grow
    }

    /// B.3 Task 4 — test-only: attach a synthetic PRESENT completion
    /// to the open frame's cow slot (via `engine.attach_present_completion`)
    /// without a real X PRESENT client. Returns `true` if the attach
    /// succeeded (the cow_id is written in the open frame's ops list),
    /// `false` if attach returned Err (no open frame, or frame doesn't
    /// write to cow_id).
    ///
    /// The synthetic entry carries `wake_pin: PinnedWake::None` and an
    /// event with `serial = synthetic_serial`; the `CompletedPresentEvent`
    /// fields other than serial are zeroed/defaulted.
    #[allow(
        dead_code,
        reason = "used by frame_builder_cow_copy_area_delivers_present_completion"
    )]
    pub fn attach_synthetic_present_completion_to_cow_for_tests(
        &mut self,
        synthetic_serial: u32,
    ) -> bool {
        use crate::kms::render::present_completion::{PendingPresentEntry, PinnedWake};
        use yserver_core::backend::{CompletedPresentEvent, PresentWake};
        use yserver_protocol::x11::ClientId;

        let cow_id = match self.cow_id {
            Some(id) => id,
            None => return false,
        };
        let entry = PendingPresentEntry {
            wake_pin: PinnedWake::None,
            event: CompletedPresentEvent {
                client_id: ClientId(0),
                serial: synthetic_serial,
                host_xid: 0,
                dst_host_xid: 0,
                options: 0,
                present_id: 0,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
        };
        self.engine.attach_present_completion(cow_id, entry).is_ok()
    }

    /// B.3 Task 4 — test-only: call `engine.drain_all` (waits on all
    /// in-flight fence tickets so PRESENT completions become Ready).
    /// Used after force-closing a frame to age out the fence ticket
    /// before asserting on `drain_completed_present_events_for_tests`.
    pub fn engine_drain_all_for_tests(&mut self) {
        self.engine.drain_all(&mut self.platform);
    }

    /// Task 8 — test-only: resolve three drawable xids, build a
    /// `MaskedCopyMask` from the mask drawable's storage (plain
    /// depth-1 drawable path, `snapshot_id: None`), and invoke
    /// `engine.masked_copy_area`. No explicit flush is needed: the
    /// subsequent `get_image_pixels_for_tests` calls
    /// `engine.get_image` which calls `flush_render_batch` +
    /// `close_open_frame` + `flush_submit_group` before the readback
    /// fence, so the masked copy is visible to the caller.
    #[allow(clippy::too_many_arguments)]
    pub fn masked_copy_area_for_tests(
        &mut self,
        src_xid: u32,
        dst_xid: u32,
        mask_xid: u32,
        clip_origin: (i32, i32),
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        w: u16,
        h: u16,
        scissors: &[vk::Rect2D],
    ) -> io::Result<()> {
        let src = self.store.lookup(src_xid).ok_or_else(|| {
            io::Error::other(format!(
                "masked_copy_area_for_tests: src xid 0x{src_xid:x} not in store"
            ))
        })?;
        let dst = self.store.lookup(dst_xid).ok_or_else(|| {
            io::Error::other(format!(
                "masked_copy_area_for_tests: dst xid 0x{dst_xid:x} not in store"
            ))
        })?;
        let mask_id = self.store.lookup(mask_xid).ok_or_else(|| {
            io::Error::other(format!(
                "masked_copy_area_for_tests: mask xid 0x{mask_xid:x} not in store"
            ))
        })?;
        let mask = {
            let md = self.store.get(mask_id).ok_or_else(|| {
                io::Error::other(format!(
                    "masked_copy_area_for_tests: mask drawable 0x{mask_xid:x} missing from entries"
                ))
            })?;
            crate::kms::render::engine::MaskedCopyMask {
                image: md.storage.image,
                view: md.storage.image_view, // IDENTITY R8 view
                old_layout: md.storage.current_layout,
                extent: md.storage.extent,
                clip_origin: [clip_origin.0, clip_origin.1],
                snapshot_id: None, // plain-drawable test path (not a snapshot)
            }
        };
        self.engine
            .masked_copy_area(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(src),
                Dst::server_internal(dst),
                vk::Offset2D {
                    x: i32::from(src_x),
                    y: i32::from(src_y),
                },
                vk::Offset2D {
                    x: i32::from(dst_x),
                    y: i32::from(dst_y),
                },
                vk::Extent2D {
                    width: u32::from(w),
                    height: u32::from(h),
                },
                mask,
                scissors,
            )
            .map_err(|e| {
                io::Error::other(format!("masked_copy_area_for_tests: engine error: {e:?}"))
            })
    }

    /// Drive a fake VT enable/disable event. Used by the VT-switch
    /// integration tests.
    pub fn inject_seat_event_for_test(&mut self, state: &mut ServerState, enable: bool) {
        use crate::vt::state::VtEventKind;
        let ev = if enable {
            VtEventKind::Enable
        } else {
            VtEventKind::Disable
        };
        self.drive_vt_event(state, ev);
    }
}
