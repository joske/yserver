use super::*;

impl KmsBackend {
    /// Stage 3f.8: allocate the default cursor sprite (16×16 black
    /// triangle, hotspot (0,0)) as a Pixmap-kind Drawable + upload
    /// the pixel data via `engine.put_image`. Registers the result
    /// on `SceneCompositor` so `build_scene` appends it at top of
    /// z. One-time setup; subsequent `define_cursor` flows (Stage 4)
    /// can replace the entry.
    pub(in crate::kms::render::backend) fn init_cursor_sprite(&mut self) -> io::Result<()> {
        // Stage 5 Phase A: bake the default-arrow record into the
        // canonical cursor maps so any DefineCursor that resolves to
        // None / unknown can fall back to it. The sprite Pixmap +
        // scene registration happen via the shared
        // `insert_cursor_record` path so subsequent client cursors
        // and the default sit on the same plumbing.
        let xid = if let Some(xid) = self.xorg_root_cursor() {
            xid
        } else {
            let xid = self.core.next_host_xid();
            let bytes = crate::kms::render::cursor::default_arrow_bgra();
            self.insert_cursor_record(
                xid,
                crate::kms::render::cursor::DEFAULT_ARROW_W,
                crate::kms::render::cursor::DEFAULT_ARROW_H,
                crate::kms::render::cursor::DEFAULT_ARROW_HOT_X,
                crate::kms::render::cursor::DEFAULT_ARROW_HOT_Y,
                bytes,
            );
            xid
        };
        self.default_cursor_xid = Some(xid);
        // Force the effective cursor to resolve against the new
        // default so the scene picks it up at boot (otherwise
        // refresh_effective_cursor short-circuits on
        // pre-default `effective_cursor_xid == None == new_xid`).
        self.effective_cursor_xid = None;
        self.refresh_effective_cursor();
        log::info!("render: default cursor sprite registered (xid 0x{xid:x})");
        Ok(())
    }

    /// Xorg's root cursor (`CreateRootCursor`, dix/cursor.c): glyph 0
    /// (`X_cursor`) of the "cursor" font over mask glyph 1, black on white.
    /// It is what the screen shows wherever no window sets a cursor.
    fn xorg_root_cursor(&mut self) -> Option<u32> {
        let (font, _) = self.open_font(None, "cursor").ok()?;
        let cursor = self.create_glyph_cursor(
            None,
            font,
            Some(font),
            0,
            1,
            (0, 0, 0),
            (0xffff, 0xffff, 0xffff),
        );
        let _ = self.close_font(None, font.as_raw());
        cursor.ok().map(CursorHandle::as_raw)
    }

    // ── Stage 5 Phase A — cursor record helpers ────────────────────

    /// Allocate a fresh CursorRecord + sprite Pixmap and register
    /// both in the canonical xid maps. Bumps `next_cursor_version`,
    /// uploads the BGRA bytes to a v2 store Pixmap (so the SW scene
    /// path can sample it), and — if the new cursor is the
    /// currently-effective one — refreshes the scene's
    /// `CursorEntry`.
    ///
    /// `bgra` length MUST equal `width * height * 4`.
    pub(in crate::kms::render::backend) fn insert_cursor_record(
        &mut self,
        xid: u32,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra: Vec<u8>,
    ) {
        debug_assert_eq!(bgra.len(), usize::from(width) * usize::from(height) * 4);
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let record = crate::kms::render::cursor::CursorRecord::new(
            width, height, hot_x, hot_y, bgra, version,
        );
        // Upload the sprite to a v2 store Pixmap so the SW scene
        // path can sample it. Best-effort: a Vk-less test fixture
        // skips the upload but still keeps the record so unit tests
        // can observe bytes / version.
        if let Some(pixmap_id) = self.allocate_cursor_sprite_pixmap(&record) {
            self.cursor_pixmaps.insert(xid, pixmap_id);
        }
        self.cursor_records.insert(xid, record);
        self.refresh_effective_cursor();
    }

    pub(in crate::kms::render::backend) fn insert_monochrome_cursor_record(
        &mut self,
        xid: u32,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra_bytes: Vec<u8>,
        color_roles: Vec<crate::kms::render::cursor::CursorColorRole>,
    ) {
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let record = crate::kms::render::cursor::CursorRecord::new_monochrome_with_bgra(
            width,
            height,
            hot_x,
            hot_y,
            bgra_bytes,
            color_roles,
            version,
        );
        if let Some(pixmap_id) = self.allocate_cursor_sprite_pixmap(&record) {
            self.cursor_pixmaps.insert(xid, pixmap_id);
        }
        self.cursor_records.insert(xid, record);
        self.refresh_effective_cursor();
    }

    /// Allocate a v2 store Pixmap matching `record`'s dims, depth-32,
    /// and upload the BGRA bytes via `engine.put_image`. Returns the
    /// fresh DrawableId. Failures (no Vk in tests, allocate failure,
    /// upload failure) return `None` — the caller keeps the record
    /// but the SW scene path won't sample the sprite for that cursor.
    fn allocate_cursor_sprite_pixmap(
        &mut self,
        record: &std::sync::Arc<crate::kms::render::cursor::CursorRecord>,
    ) -> Option<crate::kms::render::store::DrawableId> {
        let storage = match self
            .platform
            .allocate_drawable_storage(record.width, record.height, 32)
        {
            Ok(s) => s,
            Err(e) => {
                log::debug!(
                    "render cursor sprite alloc: storage failed ({}x{}, depth 32): {e:?}",
                    record.width,
                    record.height,
                );
                return None;
            }
        };
        let sprite_xid = self.core.next_host_xid();
        let id = match self.store_alloc(
            sprite_xid,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        ) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("render cursor sprite alloc: store.allocate failed: {e:?}");
                return None;
            }
        };
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            ash::vk::Offset2D::default(),
            ash::vk::Extent2D {
                width: u32::from(record.width),
                height: u32::from(record.height),
            },
            &record.bgra_bytes,
            32,
        ) {
            log::warn!("render cursor sprite alloc: put_image failed: {e:?}");
            // Drop the freshly-allocated storage cleanly so it
            // doesn't leak.
            self.store_decref_with_invalidate(id);
            return None;
        }
        Some(id)
    }

    /// Read a depth-1 X11 pixmap's pixels as R8 (1 byte per pixel,
    /// non-zero = bit set). Returns `(bytes, width, height)`. None
    /// when the pixmap isn't in the store or the engine readback
    /// fails (Vk-less fixture, format mismatch).
    ///
    /// `get_image` at depth 1 returns the X11 **wire bitmap** layout —
    /// scanlines packed to 1 bit per pixel, each row padded to 32 bits,
    /// LSBFirst (`pack_from_storage`). The cursor rasteriser
    /// (`rasterise_create_cursor`) instead wants one byte per pixel,
    /// so unpack the packed rows into a tight `w × h` R8 buffer here.
    /// Skipping this unpack collapsed a `w`-wide cursor into its first
    /// `⌈w/32⌉·4·h ÷ w` rows — the `import` crosshair showed as a
    /// flattened horizontal sliver (#90 follow-up).
    pub(in crate::kms::render::backend) fn read_cursor_depth1_pixmap(
        &mut self,
        host_xid: u32,
    ) -> Option<(Vec<u8>, u16, u16)> {
        let id = self.store.lookup(host_xid)?;
        let drawable = self.store.get(id)?;
        let extent = drawable.storage.extent;
        let w = u16::try_from(extent.width).ok()?;
        let h = u16::try_from(extent.height).ok()?;
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CursorDepth1);
        match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            Src::server_internal(id),
            rect,
            1,
        ) {
            Ok(packed) => Some((
                crate::kms::render::cursor::unpack_wire_bitmap_to_r8(&packed, w, h),
                w,
                h,
            )),
            Err(e) => {
                log::debug!(
                    "render read_cursor_depth1_pixmap: get_image failed for 0x{host_xid:x}: {e:?}"
                );
                None
            }
        }
    }

    /// Read a BGRA-mirrored X11 pixmap's pixels at depth 32.
    /// Returns `(bytes, width, height)`. None when the pixmap isn't
    /// in the store or readback fails (Vk-less fixture, format
    /// mismatch).
    pub(in crate::kms::render::backend) fn read_cursor_bgra_pixmap(
        &mut self,
        host_xid: u32,
    ) -> Option<(Vec<u8>, u16, u16)> {
        let id = self.store.lookup(host_xid)?;
        let drawable = self.store.get(id)?;
        let extent = drawable.storage.extent;
        let w = u16::try_from(extent.width).ok()?;
        let h = u16::try_from(extent.height).ok()?;
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CursorBgra);
        match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            Src::server_internal(id),
            rect,
            32,
        ) {
            Ok(bytes) => Some((bytes, w, h)),
            Err(e) => {
                log::debug!(
                    "render read_cursor_bgra_pixmap: get_image failed for 0x{host_xid:x}: {e:?}"
                );
                None
            }
        }
    }

    /// Render a single FreeType glyph from `font_xid` for use in
    /// glyph-cursor rasterisation. Returns `(pixels, w, h, lsb,
    /// top)`. Empty glyphs (e.g. SPACE) return a `(vec![0u8], 1, 1,
    /// lsb, top)` placeholder so the union-bbox math has something
    /// to work with. None when the font isn't known.
    pub(in crate::kms::render::backend) fn render_glyph_for_cursor(
        &self,
        font_xid: u32,
        ch: u16,
    ) -> Option<(Vec<u8>, i32, i32, i32, i32)> {
        let fs = self.core.fonts.get(&font_xid)?;
        let face = fs.face.borrow();
        let _ = face
            .0
            .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
        let glyph = face.0.glyph();
        let bitmap = glyph.bitmap();
        let w = bitmap.width();
        let h = bitmap.rows();
        if w <= 0 || h <= 0 {
            return Some((vec![0u8], 1, 1, glyph.bitmap_left(), glyph.bitmap_top()));
        }
        let stride = bitmap.pitch();
        let buf = bitmap.buffer();
        let wu = w as usize;
        let hu = h as usize;
        // FreeType bitmaps can be 1-bit `Mono` (FT_PIXEL_MODE_MONO,
        // MSB-first, one byte per 8 pixels) or 8-bit gray. The
        // emboldening / cursor mask consumer expects 8 bpp, so
        // unpack Mono → 0x00/0xff per pixel; copy gray verbatim.
        // Per-row stride is in BYTES; for Mono `stride` ≈ ceil(w/8).
        let mono = matches!(bitmap.pixel_mode(), Ok(freetype::bitmap::PixelMode::Mono));
        let mut pixels = vec![0u8; wu * hu];
        for row in 0..hu {
            let row_start = if stride >= 0 {
                row * stride as usize
            } else {
                (hu - 1 - row) * (stride as isize).unsigned_abs()
            };
            let dst_row = row * wu;
            if mono {
                for col in 0..wu {
                    let byte = buf.get(row_start + (col >> 3)).copied().unwrap_or(0);
                    pixels[dst_row + col] = if byte & (0x80 >> (col & 7)) != 0 {
                        0xff
                    } else {
                        0
                    };
                }
            } else {
                let end = row_start + wu;
                if end <= buf.len() {
                    pixels[dst_row..dst_row + wu].copy_from_slice(&buf[row_start..end]);
                }
            }
        }
        Some((pixels, w, h, glyph.bitmap_left(), glyph.bitmap_top()))
    }

    /// Walk the parent chain from `host_xid` upward, returning the
    /// first non-None cursor attribute encountered. Falls back to
    /// `core.active_cursor` (the sticky DefineCursor-on-root) if
    /// the chain runs out — that is, no window on the chain bound a
    /// cursor.
    pub(in crate::kms::render::backend) fn effective_cursor_walking_chain(
        &self,
        host_xid: u32,
    ) -> Option<u32> {
        // An active pointer grab's cursor (Xorg `ActivatePointerGrab`)
        // is the highest-priority sprite: it wins over the per-window
        // chain and the sticky/default fallback for the grab's
        // duration.
        if let Some(handle) = self.grab_cursor_override {
            return Some(handle);
        }
        let mut cur = host_xid;
        // Bound the walk so a corrupted parent loop can't burn the
        // event loop. windows fits in u32 xids; 64 is generous.
        for _ in 0..64 {
            if let Some(io) = self.input_only_pointer_hosts.get(&cur) {
                if let Some(c) = io.cursor {
                    return Some(c);
                }
                cur = io.parent_host;
                continue;
            }
            if let Some(geom) = self.windows.get(&cur) {
                if let Some(c) = geom.cursor {
                    return Some(c);
                }
                if let Some(p) = geom.parent {
                    cur = p;
                    continue;
                }
            }
            break;
        }
        self.core.active_cursor.or(self.default_cursor_xid)
    }

    /// Recompute the effective cursor for the window currently under
    /// the pointer and swap the scene `CursorEntry` if it changed.
    /// Cheap when the choice is stable (HashMap lookup + Option
    /// compare).
    pub(in crate::kms::render::backend) fn refresh_effective_cursor(&mut self) {
        let pointer_window = self.core.prev_pointer_window.unwrap_or(self.core.window_id);
        let new_xid = self.effective_cursor_walking_chain(pointer_window);
        if new_xid == self.effective_cursor_xid {
            // Same effective cursor — a running animation keeps its
            // frame index (Xorg: "already current → do nothing").
            return;
        }
        self.effective_cursor_xid = new_xid;
        self.sync_cursor_animation(new_xid);
        // XFIXES CursorNotify (Xorg `CursorDisplayCursor`: the requested
        // cursor differs from the sprite's current one). Queued AFTER the
        // animation re-arm so the serial is the one `GetCursorImage` now
        // reports. A missing cursor reports serial 0, as Xorg does for a
        // NULL cursor.
        self.displayed_cursor_pending = Some(yserver_core::backend::DisplayedCursor {
            host_xid: new_xid.unwrap_or(0),
            serial: new_xid
                .and_then(|xid| self.cursor_records.get(&xid))
                .map_or(0, |record| {
                    u32::try_from(record.version).unwrap_or(u32::MAX)
                }),
        });
        // The sprite's reference moved: the cursor it showed may be gone.
        self.collect_released_cursors();
        let Some(xid) = new_xid else {
            return;
        };
        self.display_cursor_by_handle(xid);
    }

    /// Whether anything besides a cursor XID still references host cursor
    /// `xid` — the references Xorg counts in `pCurs->refcnt`: window cursor
    /// attributes (`dix/window.c:1536`), the grab (`dix/grabs.c:243`), the
    /// sprite's current cursor (`dix/events.c:954`), an animated cursor's
    /// frames (`render/animcur.c:360`); plus our sticky root default. An
    /// InputOnly window has no backend window: the core holds its cursor.
    fn cursor_referenced(&self, xid: u32) -> bool {
        self.effective_cursor_xid == Some(xid)
            || self.default_cursor_xid == Some(xid)
            || self.core.active_cursor == Some(xid)
            || self.grab_cursor_override == Some(xid)
            || self.windows.values().any(|geom| geom.cursor == Some(xid))
            || self
                .anim_cursor_records
                .values()
                .any(|anim| anim.frames.iter().any(|frame| frame.source == xid))
    }

    /// Destroy every released cursor nothing references any more, and its
    /// sprite pixmap. An animated cursor's canonical sprite aliases a frame's
    /// pixmap, so only a static cursor frees one; dropping an animated cursor
    /// can release its frames, hence the loop.
    pub(in crate::kms::render::backend) fn collect_released_cursors(&mut self) {
        loop {
            let Some(xid) = self
                .released_cursors
                .iter()
                .copied()
                .find(|&xid| !self.cursor_referenced(xid))
            else {
                return;
            };
            self.released_cursors.remove(&xid);
            self.cursor_records.remove(&xid);
            let sprite = self.cursor_pixmaps.remove(&xid);
            if self.anim_cursor_records.remove(&xid).is_none()
                && let Some(id) = sprite
            {
                self.store_decref_with_invalidate(id);
            }
        }
    }

    /// XFIXES `HideCursor` / `ShowCursor` edge. Hiding drops the scene's
    /// cursor entry (the scene then assigns `Hidden` on every output, which
    /// detaches a bound HW plane on the next retire) and, while a direct
    /// root scanout owns the planes, detaches the legacy cursor plane
    /// right away since no compose will run. Showing re-displays the
    /// current effective cursor through the ordinary path.
    pub(in crate::kms::render::backend) fn apply_cursor_hidden(&mut self, hidden: bool) {
        if self.cursor_hidden == hidden {
            return;
        }
        self.cursor_hidden = hidden;
        if hidden {
            self.scene.clear_cursor();
            if self.scanout_m2.active() {
                for output_idx in 0..self.platform.outputs.len() {
                    if let Err(error) = self.platform.cursor_plane_hide_on_crtc(output_idx) {
                        log::warn!(
                            "scanout_m2: cursor hide failed on output {output_idx}: {error}; unflipping"
                        );
                        self.request_direct_unflip("cursor_hide_failed");
                        return;
                    }
                }
            }
            return;
        }
        let Some(xid) = self.effective_cursor_xid else {
            return;
        };
        self.display_cursor_by_handle(xid);
        // Under direct scanout nothing composes, and the scene's cursor
        // mode may still read Hidden, so `display_cursor_by_handle` can
        // skip the plane. Rebind it directly.
        if self.scanout_m2.active()
            && let Some(record) = self.cursor_records.get(&xid).cloned()
            && !self.refresh_direct_cursor_on_all_outputs(&record)
        {
            self.request_direct_unflip("cursor_show_failed");
        }
    }

    /// Arm (reset to frame 0) or clear the cursor animation for the
    /// new effective cursor. Arming swaps the canonical maps to
    /// frame 0 under a freshly-minted version so the XFixes serial
    /// stays monotonic (spec "Version/serial"). Clear fires both for
    /// `new_xid = None` and for a non-animated cursor (no
    /// `anim_cursor_records` entry).
    fn sync_cursor_animation(&mut self, new_xid: Option<u32>) {
        let Some(xid) = new_xid else {
            self.active_cursor_anim = None;
            return;
        };
        let Some(anim) = self.anim_cursor_records.get(&xid) else {
            self.active_cursor_anim = None;
            return;
        };
        let first = &anim.frames[0];
        let (record, pixmap, delay) = (
            std::sync::Arc::clone(&first.record),
            first.pixmap,
            first.delay,
        );
        self.swap_anim_frame_into_maps(xid, &record, pixmap);
        self.active_cursor_anim = Some(crate::kms::render::cursor::ActiveCursorAnim {
            handle: xid,
            frame: 0,
            next_frame: std::time::Instant::now() + delay,
        });
    }

    /// Advance the running cursor animation if its deadline elapsed.
    /// Called from `maybe_composite` AFTER its scanout/DPMS gates
    /// (spec "Frame tick"). One advance per call — a stale deadline
    /// after a blank advances a single frame, never fast-forwards.
    pub(crate) fn tick_cursor_animation(&mut self) {
        if !self.kms_outputs_active || !self.scanout_allowed() {
            return;
        }
        let now = std::time::Instant::now();
        let Some(st) = self.active_cursor_anim.as_ref() else {
            return;
        };
        if now < st.next_frame {
            return;
        }
        let handle = st.handle;
        let current = st.frame;
        let Some(anim) = self.anim_cursor_records.get(&handle) else {
            self.active_cursor_anim = None;
            return;
        };
        let next = (current + 1) % anim.frames.len();
        let frame = &anim.frames[next];
        let (record, pixmap, delay) = (
            std::sync::Arc::clone(&frame.record),
            frame.pixmap,
            frame.delay,
        );
        self.swap_anim_frame_into_maps(handle, &record, pixmap);
        // Always Some here — checked at the top and nothing in between clears it.
        if let Some(st) = self.active_cursor_anim.as_mut() {
            st.frame = next;
            st.next_frame = now + delay;
        }
        self.display_cursor_by_handle(handle);
    }

    /// Deadline for `next_wakeup`: the animation's next frame, only
    /// while it could actually be displayed (same gates as the tick).
    pub(in crate::kms::render::backend) fn cursor_anim_deadline(
        &self,
    ) -> Option<std::time::Instant> {
        if !self.kms_outputs_active || !self.scanout_allowed() {
            return None;
        }
        self.active_cursor_anim.as_ref().map(|st| st.next_frame)
    }

    /// Re-point the canonical maps at an animation frame under a
    /// freshly-minted monotonic version. The byte clone is bounded
    /// by cursor size (≤16 KiB for HW-plane cursors).
    fn swap_anim_frame_into_maps(
        &mut self,
        xid: u32,
        record: &std::sync::Arc<crate::kms::render::cursor::CursorRecord>,
        pixmap: Option<crate::kms::render::store::DrawableId>,
    ) {
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let minted = crate::kms::render::cursor::CursorRecord::new(
            record.width,
            record.height,
            record.hot_x,
            record.hot_y,
            record.bgra_bytes.clone(),
            version,
        );
        self.cursor_records.insert(xid, minted);
        // Keep cursor_pixmaps truthful per-frame: a `None` frame
        // REMOVES the entry — leaving the prior frame's pixmap
        // installed would have the SW scene path sample stale bytes.
        // Note `display_cursor_by_handle` early-returns on a missing
        // pixmap entry, so a `None` frame skips display entirely
        // (HW upload included) — `None` only occurs in Vk-less
        // fixtures or after a sprite-alloc failure.
        match pixmap {
            Some(p) => {
                self.cursor_pixmaps.insert(xid, p);
            }
            None => {
                self.cursor_pixmaps.remove(&xid);
            }
        }
    }

    /// Push `cursor_records[xid]` to the scene / HW plane — the
    /// former tail of `refresh_effective_cursor`, shared with the
    /// animation tick. Keeps the sample-view readiness guard
    /// (Vk-less fixtures build records without sprite allocs).
    pub(in crate::kms::render::backend) fn display_cursor_by_handle(&mut self, xid: u32) {
        // XFIXES HideCursor: the effective cursor (and a running
        // animation) keep advancing, nothing reaches the screen.
        // `apply_cursor_hidden(false)` re-displays the current one.
        if self.cursor_hidden {
            return;
        }
        let Some(record) = self.cursor_records.get(&xid).cloned() else {
            return;
        };
        let Some(&pixmap_id) = self.cursor_pixmaps.get(&xid) else {
            return;
        };
        // Sample-view readiness check — same gate as Stage 3f.8's
        // boot path. A Vk-less fixture builds the record but skips
        // the sprite alloc, so this short-circuits cleanly.
        if self
            .store
            .get(pixmap_id)
            .map(|d| d.storage.image_view == ash::vk::ImageView::null())
            .unwrap_or(true)
        {
            return;
        }
        // Parameter changes can make a previously-EINVAL cursor bind valid.
        // Notify every device even while all outputs are temporarily using SW;
        // record version/pixel-only animation changes intentionally do not
        // reset the per-CRTC backoff.
        self.platform.cursor_plane_note_sprite_hotspot(
            record.width,
            record.height,
            record.hot_x,
            record.hot_y,
        );
        self.scene
            .register_cursor(crate::kms::render::scene::CursorEntry {
                id: pixmap_id,
                extent: ash::vk::Extent2D {
                    width: u32::from(record.width),
                    height: u32::from(record.height),
                },
                hot_x: i16::try_from(record.hot_x).unwrap_or(i16::MAX),
                hot_y: i16::try_from(record.hot_y).unwrap_or(i16::MAX),
                record_version: record.version,
                bgra_bytes: Some(std::sync::Arc::new(record.bgra_bytes.clone())),
            });

        // Stage 5 Phase D — steady-state HW sprite-change. When the
        // plane is fully bound, the scene won't re-tick (v2's
        // empty-damage fast path at scene.rs:840), so a record
        // swap would starve the upload waiting for a compose
        // event. Push the bytes synchronously through the scene's
        // queueing path; if any output is transitioning, the
        // bytes land in the deferred slot until the wait set
        // drains.
        if matches!(
            self.scene.cursor_mode(),
            crate::kms::render::scene::CursorPlaneMode::Hw
                | crate::kms::render::scene::CursorPlaneMode::Mixed
        ) {
            // Direct root scanout does not need a composed primary-plane
            // retirement merely to replace cursor pixels. Update the legacy
            // cursor planes in place; if any output rejects the operation,
            // fall through to the scene state machine and unwind M2 so its
            // ordinary per-output retry/fallback can retire safely.
            if self.scanout_m2.active() && self.refresh_direct_cursor_on_all_outputs(&record) {
                return;
            }
            let bytes = std::sync::Arc::new(record.bgra_bytes.clone());
            #[allow(clippy::cast_possible_truncation)]
            let cx = self.core.cursor_x as i32;
            #[allow(clippy::cast_possible_truncation)]
            let cy = self.core.cursor_y as i32;
            let refreshes_hw_binding = self.scene.queue_steady_state_cursor_upload(
                &mut self.platform,
                record.version,
                record.width,
                record.height,
                bytes,
                record.hot_x,
                record.hot_y,
                cx,
                cy,
            );
            if refreshes_hw_binding && self.scanout_m2.active() {
                self.request_direct_unflip("direct_cursor_refresh_failed");
            }
        }
        // The scene blit ordering: register_cursor already marks
        // scene_structure_dirty so the next tick repaints; no extra
        // wake needed.
    }
}
