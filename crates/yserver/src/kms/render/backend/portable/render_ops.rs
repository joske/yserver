use super::*;

impl KmsBackend {
    /// Resolve a RENDER source / mask picture into what the engine samples.
    ///
    /// Takes `&self` because a picture on a WINDOW must go through
    /// `resolve_paint_target`: see the `PictureRecord::Drawable` arm.
    pub(in crate::kms::render::backend) fn resolve_picture_for_render(
        &self,
        host_pic: u32,
    ) -> Option<(
        crate::kms::render::engine::ResolvedSource,
        Repeat,
        Option<PictTransform>,
        bool, // component_alpha
    )> {
        use crate::kms::render::engine::{ResolvedSource, SourceDrawable};
        match self.core.pictures.get(&host_pic)? {
            PictureRecord::Drawable {
                host_xid,
                repeat,
                transform,
                component_alpha,
                ..
            } => {
                // #133 step 3 (P4) — a picture wrapping a WINDOW resolves
                // through `resolve_paint_target`, exactly like every other
                // route to a window's pixels (`copy_area`'s source,
                // `copy_plane`'s source, every destination). That hands back
                // all three things this needs:
                //
                // - the drawable that HOLDS the pixels. A redirected
                //   window's leaf storage is stale — its pixels live in the
                //   backing — so a raw `store.lookup` samples the wrong
                //   image. Same premise as `copy_plane`.
                // - the ACCUMULATED content origin. A window's content
                //   starts `bw` inside its own storage (`compAllocPixmap`,
                //   `composite/compalloc.c:610`), and a child below a
                //   redirected ancestor additionally carries the whole
                //   `W.bw + C.x + C.bw` chain the resolver walks — a single
                //   level's `border_width` is not enough.
                // - the read bounds, via the window's own extent as the
                //   source domain (below).
                //
                // Xorg arrives at the same offset from the other side:
                // `create_bits_picture` builds the pixman image over the
                // whole backing pixmap (`fb/fbpict.c:293-296`) and then adds
                // `pict->pDrawable->x/y` to the sampling offset
                // (`fb/fbpict.c:328-329`), which for a redirected bordered
                // window is exactly `bw` because `compAllocPixmap` places
                // the pixmap at `screen_x = drawable.x - bw`.
                //
                // A picture wrapping a COMPOSITE-NAMED WINDOW PIXMAP is the
                // other case and must NOT be treated this way: that pixmap
                // IS the bordered image, so its border is part of the
                // drawable on purpose. Named window pixmaps are registered
                // as Pixmaps under their own xid and are absent from
                // `windows`, so the discriminator stays "is this xid a
                // window in the geometry mirror" — and the non-window arm
                // keeps its pre-#133 raw-leaf lookup, which also leaves
                // root-drawable pictures (root is not in `windows`) on the
                // routing they had.
                let source = match self.windows.get(host_xid) {
                    Some(g) => {
                        let target = self.resolve_paint_target(*host_xid)?;
                        SourceDrawable::content(
                            target.backing_id(),
                            target.offset(),
                            ash::vk::Extent2D {
                                width: u32::from(g.width),
                                height: u32::from(g.height),
                            },
                        )
                    }
                    None => SourceDrawable::whole(self.store.lookup(*host_xid)?),
                };
                Some((
                    ResolvedSource::Drawable(source),
                    *repeat,
                    *transform,
                    *component_alpha,
                ))
            }
            PictureRecord::SolidFill {
                premul,
                repeat,
                component_alpha,
            } => Some((
                ResolvedSource::Solid(*premul),
                *repeat,
                None,
                *component_alpha,
            )),
            PictureRecord::LinearGradient {
                repeat, transform, ..
            }
            | PictureRecord::RadialGradient {
                repeat, transform, ..
            } => {
                // Stage 3f.13: full LUT sampling. The engine-side
                // `GradientPicture` was built at create time and lives
                // in `engine.picture_paint[host_pic]`; engine looks it
                // up by xid. If the engine-side build failed (test
                // fixture with no Vk, or allocation error), the engine
                // logs a gap and skips the paint — no first-stop
                // collapse fallback.
                Some((
                    ResolvedSource::Gradient(host_pic),
                    *repeat,
                    *transform,
                    false,
                ))
            }
        }
    }

    /// #137 visibility note — record a `CompositeGlyphs` this server
    /// cannot serve: bump the counter, and log the FIRST occurrence at
    /// `warn!` with every later one at `debug!`.
    ///
    /// A drop here draws nothing and returns no error, so the client
    /// cannot tell — and neither could we. #137 was an entire class of
    /// application (every Java/AWT one) rendering no text at all while
    /// the server's own telemetry knew, because the only evidence sat
    /// at `debug` in a module the usual `RUST_LOG` filters exclude.
    /// Warning once means a silently-unsupported paint path announces
    /// itself rather than never; reverting to `debug` after that means
    /// a pathological client cannot flood the log.
    ///
    /// **Once per process**, deliberately and literally — not once per
    /// server generation. A bare flag does not reset itself at a
    /// generation boundary, and wiring one into the reset lifecycle
    /// would depend on the #121 reset work, which is unmerged and
    /// parked. Revisit if and when reset lands.
    ///
    /// The counter is NOT rate-limited: it advances on every drop.
    pub(in crate::kms::render::backend) fn record_composite_glyphs_drop(
        &mut self,
        reason: std::fmt::Arguments<'_>,
    ) {
        self.telemetry.record_composite_glyphs_dropped_unsupported();
        if take_first_occurrence(&COMPOSITE_GLYPHS_DROP_WARNED) {
            log::warn!(
                "render composite_glyphs UNSUPPORTED: {reason} — this request drew \
                 NOTHING and returned no error. Further occurrences log at debug; \
                 the composite_glyphs_dropped_unsupported counter keeps counting."
            );
        } else {
            log::debug!("render composite_glyphs gap: {reason}");
        }
    }

    /// #137 tier 1 — collapse a uniform drawable glyph source to the
    /// premultiplied colour it is, by reading its single pixel.
    ///
    /// `Err` carries the reason the caller must log: every way this can
    /// decline draws no text, and a client whose text silently vanishes
    /// cannot tell.
    ///
    /// Two things about the read are load-bearing:
    ///
    /// - It goes through [`RenderEngine::get_image`], the synchronous
    ///   readback, for its flush / close-frame / fence-wait ordering. A
    ///   direct image map offers no such guarantee and would race the
    ///   client's own recolouring — Java repaints this very pixmap to
    ///   change text colour and then reuses the picture, so an unordered
    ///   read renders the *previous* colour.
    /// - It goes through [`Src::server_internal`], the privileged
    ///   unclipped backing-space handle. `SourceDrawable::offset()` is
    ///   already resolved into backing space, so a client-bounded `Src`
    ///   would reapply a coordinate space on top of it and sample the
    ///   wrong pixel of a redirected window's backing — precisely the
    ///   case the domain-not-storage rule exists to get right.
    ///
    /// Step 5 puts [`Self::uniform_glyph_source_cache`] in front of the
    /// read. That is not a copy-cost optimisation: the copy-out measured
    /// at 1.5 ms/s. It is there because `get_image` must
    /// `close_open_frame(CloseReason::SyncWait)` before it can wait, and
    /// that close measured as ~75% of ALL frame closes on a text-heavy
    /// workload (`opens=237 closes=238`, `sync_wait=179`), collapsing
    /// `ops/frame_avg` to 1.6 and destroying the batching
    /// `composite_glyphs_via_frame_builder` exists to provide. A hit
    /// skips the read and therefore skips the close.
    ///
    /// The cache is populated only on the way OUT, after the pixel has
    /// been read, and a hit requires the source's `content_version` to
    /// be unchanged — so a client that repaints the 1x1 pixmap to
    /// recolour its text (Java, every string) misses and re-reads.
    pub(in crate::kms::render::backend) fn uniform_glyph_source_premul(
        &mut self,
        host_src: u32,
        src: crate::kms::render::engine::SourceDrawable,
        repeat: Repeat,
        mask_fmt: u32,
    ) -> Result<[f32; 4], &'static str> {
        use crate::kms::render::{
            engine::{premul_from_wire_pixel, uniform_pixel_glyph_source},
            telemetry::GetImageSite,
        };
        let (depth, extent, content_version) = self
            .store
            .get(src.id())
            .map(|d| (d.depth, d.storage.extent, d.content_version))
            .ok_or("source drawable is not in the store")?;
        let rect = uniform_pixel_glyph_source(src, repeat, extent, mask_fmt)
            .ok_or("not a one-pixel sampled domain under a plane-covering repeat (tier 1 only)")?;
        // Step 5 — the cache lookup, and the ONLY thing that can skip
        // the readback below. `rect.offset` is the pixel actually read,
        // which is `src.offset()` resolved into backing space.
        let key_offset = (rect.offset.x, rect.offset.y);
        if let Some(premul) =
            self.uniform_glyph_source_cache
                .get(src.id(), content_version, key_offset)
        {
            self.telemetry.record_uniform_glyph_source_cache(true);
            return Ok(premul);
        }
        self.telemetry.record_uniform_glyph_source_cache(false);
        // The picture's DECLARED format, which overrides storage depth
        // when it says the alpha byte is padding — the same precedence
        // `resolve_force_opaque_pict_format` applies on the sampling
        // path. Absent (a synthesized source) falls back to depth.
        let pict_format = match self.core.pictures.get(&host_src) {
            Some(PictureRecord::Drawable { pict_format, .. }) => *pict_format,
            _ => 0,
        };
        self.telemetry
            .record_get_image_site(GetImageSite::GlyphSource);
        let wire = self
            .engine
            .get_image(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(src.id()),
                rect,
                depth,
            )
            .map_err(|e| {
                log::warn!(
                    "render composite_glyphs: uniform source readback failed for \
                     0x{host_src:x} at {rect:?} d{depth}: {e:?}"
                );
                "uniform source readback failed"
            })?;
        let premul = premul_from_wire_pixel(&wire, depth, pict_format)
            .ok_or("source depth has no RENDER pixel decode, or the read came back short")?;
        // Populate AFTER the ordered read, never before — see spec
        // invariant 4 and [`UniformGlyphSourceCache`]. The version read
        // above is the one the bytes just returned belong to: nothing
        // between it and here can write the source, because this thread
        // IS the request loop and the read is fence-waited.
        self.uniform_glyph_source_cache
            .insert(src.id(), content_version, key_offset, premul);
        Ok(premul)
    }

    /// Read a root-window region from the composited scanout, assembling across
    /// outputs. Returns the ZPixmap bytes (row-major, 4 bytes/pixel) for
    /// `region`, or `None` when there is no scanout (no KMS outputs) so the
    /// caller can fall through to the empty-reply path. Uncovered / failed
    /// pieces are zero-filled. See [`assemble_root_scanout`] for why the plain
    /// GetImage path must split like `CopyArea` does.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::kms::render::backend) fn render_composite_inner(
        &mut self,
        inferiors_snapshot: Option<u32>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        use crate::kms::render::engine::ResolvedSource;
        if width == 0 || height == 0 {
            return Ok(Vec::new());
        }
        let Some((mut src_resolved, src_repeat, src_transform, _src_ca)) =
            self.resolve_picture_for_render(host_src)
        else {
            log::debug!("render render_composite gap: host_src 0x{host_src:x} not resolvable");
            return Ok(Vec::new());
        };
        if let Some(xid) = inferiors_snapshot
            && let Some(id) = self.store.lookup(xid)
        {
            src_resolved =
                ResolvedSource::Drawable(crate::kms::render::engine::SourceDrawable::whole(id));
        }
        let (mask_resolved, mask_repeat, mask_transform, mask_component_alpha) = if host_mask == 0 {
            (ResolvedSource::None, Repeat::None, None, false)
        } else {
            let Some(t) = self.resolve_picture_for_render(host_mask) else {
                log::debug!(
                    "render render_composite gap: host_mask 0x{host_mask:x} not resolvable"
                );
                return Ok(Vec::new());
            };
            t
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!(
                "render render_composite gap: host_dst 0x{host_dst:x} not a Drawable picture"
            );
            return Ok(Vec::new());
        };
        // Stage 4a — resolve through redirect routing. The picture
        // wraps a window xid; the actual paint may land in that
        // window's COMPOSITE backing with an accumulated offset.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_composite gap: dst drawable 0x{dst_host_xid:x} \
                 not in store (post-resolve)"
            );
            return Ok(Vec::new());
        };
        let clip_by_children = dst_picture_clip_by_children(&self.core, host_dst);
        let dst_local_extent = self.dst_local_extent(dst_host_xid, dst_target.backing_id());
        let op_bbox_local = Rectangle16 {
            x: dst_x,
            y: dst_y,
            width,
            height,
        };
        let cliplist_local = self.render_dst_cliplist_local(
            dst_host_xid,
            clip_by_children,
            dst_clip.as_deref(),
            dst_local_extent,
            op_bbox_local,
        );
        if cliplist_local.is_empty() {
            return Ok(Vec::new());
        }
        let dst_clip =
            Self::shift_dst_picture_clip(Some(cliplist_local.clone()), dst_target.offset());

        // Audit #2 (2026-05-19) — fold src/mask client clips into
        // the composite-region clip per Xorg's
        // `miComputeCompositeRegion` (`render/mipict.c:316-389`).
        // Pre-fix, `resolve_picture_for_render` discarded src/mask
        // clips entirely, so `SetPictureClipRectangles` on a source
        // picture (xfwm4/muffin shadow blits) painted over the
        // whole dst. The translation offset matches Xorg's
        // `miClipPictureSrc(..., xDst - xSrc, yDst - ySrc)` call
        // site at `mipict.c:356,370` — the dst already has
        // `dst_target.offset()` applied to `(xDst, yDst)`, so the
        // translation picks up that offset automatically.
        // #133 step 3 (P4): the source/mask DOMAIN joins the client
        // clip. For a bordered window source that is what keeps a
        // `RepeatNone` sample outside the window from returning a ring
        // texel — see `picture_source_domain_clip`. `None` unless the
        // picture wraps a bordered window, so `bw == 0` folds exactly
        // what it folded before.
        let fold_domain = |client: Option<Vec<Rectangle16>>,
                           domain: Option<Vec<Rectangle16>>|
         -> Option<Vec<Rectangle16>> {
            match (client, domain) {
                (Some(c), Some(d)) => Some(intersect_clip_lists(&c, &d)),
                (Some(c), None) => Some(c),
                (None, d) => d,
            }
        };
        let src_clip = fold_domain(
            picture_client_clip(&self.core, host_src),
            picture_source_domain_clip(
                &self.store,
                &src_resolved,
                src_repeat,
                src_transform.as_ref(),
            ),
        );
        let mask_clip = if host_mask == 0 {
            None
        } else {
            fold_domain(
                picture_client_clip(&self.core, host_mask),
                picture_source_domain_clip(
                    &self.store,
                    &mask_resolved,
                    mask_repeat,
                    mask_transform.as_ref(),
                ),
            )
        };
        let dst_origin_x = i32::from(dst_x) + dst_target.offset().0;
        let dst_origin_y = i32::from(dst_y) + dst_target.offset().1;
        let src_translation = (
            dst_origin_x - i32::from(src_x),
            dst_origin_y - i32::from(src_y),
        );
        let mask_translation = (
            dst_origin_x - i32::from(mask_x),
            dst_origin_y - i32::from(mask_y),
        );
        let dst_clip = compute_render_composite_clip(
            dst_clip.as_deref(),
            src_clip.as_deref(),
            src_translation,
            mask_clip.as_deref(),
            mask_translation,
        );

        let rect = crate::kms::vk::ops::render::CompositeRect {
            src_x: i32::from(src_x),
            src_y: i32::from(src_y),
            mask_x: i32::from(mask_x),
            mask_y: i32::from(mask_y),
            dst_x: i32::from(dst_x) + dst_target.offset().0,
            dst_y: i32::from(dst_y) + dst_target.offset().1,
            width: u32::from(width),
            height: u32::from(height),
        };
        // Audit #4 (2026-05-19) — thread src/mask/dst PictFormat IDs
        // through to the engine so an xRGB32 picture wrapping a
        // depth-32 storage picks a no-alpha sample swizzle +
        // force-opaque for sources, AND the right "no alpha target"
        // pipeline + readback selection for destinations.
        // `picture_pict_format` returns 0 for non-Drawable picture
        // variants and unknown xids — engine falls back to the depth
        // heuristic in those cases.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let mask_pict_format = picture_pict_format(&self.core, host_mask);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        let stats = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            mask_resolved,
            dst_target.dst(),
            std::slice::from_ref(&rect),
            dst_clip.as_deref(),
            src_repeat,
            mask_repeat,
            src_transform,
            mask_transform,
            mask_component_alpha,
            src_pict_format,
            mask_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let mask_class = if host_mask == 0 {
            None
        } else {
            Some(self.picture_src_class_by_xid(host_mask))
        };
        match &stats {
            Ok(s) => {
                if s.recorded_draws > 0 && !s.deferred_to_batch {
                    self.telemetry.record_paint_submit();
                    self.trace_render(
                        SubmitKind::RenderComposite,
                        dst_target.backing_id(),
                        s.recorded_draws,
                        op,
                        src_class,
                        mask_class,
                        SubmitFlags {
                            readback: s.used_dst_readback,
                            alias: s.used_src_alias_scratch,
                            zero_draws: false,
                            upload: false,
                        },
                    );
                }
                if s.used_dst_readback {
                    self.telemetry.record_disjoint_readback();
                }
                log::trace!(
                    target: "yserver::kms::render::render",
                    "render_composite stats dst=0x{host_dst:x} \
                     recorded_draws={} used_src_alias_scratch={} used_dst_readback={}",
                    s.recorded_draws,
                    s.used_src_alias_scratch,
                    s.used_dst_readback,
                );
            }
            Err(e) => {
                log::warn!("render render_composite: engine returned {e:?} on dst 0x{host_dst:x}");
            }
        }
        // Phase B.2 Task 15: render_composite may open a frame; drain
        // any resulting close events into telemetry so the per-second
        // emit picks them up without stale lag. Mirrors the B.1 drain
        // at the composite_glyphs wrapper.
        self.drain_frame_builder_telemetry();
        Ok(local_rects_to_region(cliplist_local))
    }

    /// What a RENDER source Picture on a window reads, as a sampleable
    /// drawable, when the window's own storage does not hold it: Xorg
    /// samples the pixmap the window draws into, its inferiors and all,
    /// whatever the picture's subwindow mode — only a client clip limits
    /// a source (`miClipPictureSrc`, `render/mipict.c:265-284`; fb builds
    /// it without a composite clip, `fb/fbpict.c:57`). Measured by
    /// tools/vng-scenarios/draw-clip-probe.c in both modes.
    ///
    /// The root: its own storage holds only the backdrop (#135; maim's
    /// whole capture is one such Composite), so this borrows the read
    /// `GetImage` of the root does (`read_root_scanout_assembled`) and
    /// uploads it into a short-lived pixmap. Any other window: its storage
    /// with [`Self::inferior_pieces`] copied over it, when there are any.
    /// `None` leaves the normal source routing untouched.
    ///
    /// The scratch pixmap is freed by the caller through the ordinary
    /// `free_pixmap` path, so its storage retires behind the fence like any
    /// other drawable rather than being destroyed under in-flight GPU work.
    pub(in crate::kms::render::backend) fn source_inferiors_snapshot(
        &mut self,
        host_pic: u32,
    ) -> Option<u32> {
        let Some(PictureRecord::Drawable { host_xid, .. }) = self.core.pictures.get(&host_pic)
        else {
            return None;
        };
        let host_xid = *host_xid;
        if host_xid != self.core.window_id {
            return self.window_inferiors_snapshot(host_xid, None);
        }
        let root_xid = self.core.window_id;
        let root_id = self.store.lookup(root_xid)?;
        let (extent, depth) = {
            let d = self.store.get(root_id)?;
            (d.storage.extent, d.depth)
        };
        if extent.width == 0 || extent.height == 0 {
            return None;
        }
        let region = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let bytes = self.read_root_scanout_assembled(region)?;
        let width = u16::try_from(extent.width).ok()?;
        let height = u16::try_from(extent.height).ok()?;
        let scratch = self.create_pixmap(None, depth, width, height).ok()?;
        let scratch_xid = scratch.as_raw();
        let Some(target) = self.resolve_paint_target(scratch_xid) else {
            let _ = self.free_pixmap(None, scratch_xid);
            return None;
        };
        // PRIVILEGED whole-backing write: this is a server-internal upload of
        // the composited screen, not a client paint, and the scratch drawable
        // has no border so the two targets coincide anyway.
        self.put_image_rop_cpu(
            target.server_backing_dst(),
            vk::Offset2D { x: 0, y: 0 },
            width,
            (0, 0),
            width,
            height,
            &bytes,
            depth,
            yserver_core::backend::GcFunction::Copy,
            depth_plane_mask(depth),
        );
        Some(scratch_xid)
    }

    /// [`Self::source_inferiors_snapshot`] for a window other than the
    /// root: `area` of it (its content space; `None` for all of it) as a
    /// scratch pixmap whose `(0, 0)` is `area`'s origin. `None` when its
    /// storage holds all it shows there already.
    pub(in crate::kms::render::backend) fn window_inferiors_snapshot(
        &mut self,
        host_xid: u32,
        area: Option<vk::Rect2D>,
    ) -> Option<u32> {
        let geom = *self.windows.get(&host_xid)?;
        let target = self.resolve_paint_target(host_xid)?;
        let whole = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: u32::from(geom.width),
                height: u32::from(geom.height),
            },
        };
        let area = area.unwrap_or(whole);
        let content = intersect_rect_with_clip(area, &[whole])
            .into_iter()
            .next()?;
        let mut pieces = Vec::new();
        self.inferior_pieces(
            host_xid,
            (0, 0),
            &[content],
            target.backing_id(),
            &mut pieces,
        );
        if pieces.is_empty() {
            return None;
        }
        let scratch = self
            .create_pixmap(
                None,
                geom.depth,
                u16::try_from(area.extent.width).ok()?,
                u16::try_from(area.extent.height).ok()?,
            )
            .ok()?
            .as_raw();
        let Some(dst) = self.resolve_paint_target(scratch) else {
            let _ = self.free_pixmap(None, scratch);
            return None;
        };
        let copy = |b: &mut Self, src: PaintTarget, origin: (i32, i32), r: vk::Rect2D| {
            let src_rect = vk::Rect2D {
                offset: vk::Offset2D {
                    x: r.offset.x - origin.0 + src.offset().0,
                    y: r.offset.y - origin.1 + src.offset().1,
                },
                extent: r.extent,
            };
            let at = vk::Offset2D {
                x: r.offset.x - area.offset.x,
                y: r.offset.y - area.offset.y,
            };
            if let Err(e) = b.engine.copy_area(
                &mut b.store,
                &mut b.platform,
                src.src_including_border(),
                dst.server_backing_dst(),
                src_rect,
                at,
            ) {
                log::debug!("render source snapshot of {host_xid:#x}: copy {r:?}: {e:?}");
            }
        };
        copy(self, target, (0, 0), content);
        for piece in pieces {
            for r in &piece.rects {
                copy(self, piece.target, piece.origin, *r);
            }
        }
        Some(scratch)
    }
}

/// Parse gradient stops (Stage 3b helper shared by linear +
/// radial). `stops_offset` is the offset in `body` where the
/// `n_stops` u32 starts. Returns `None` if the body is short.
/// Stops carry pos (FIXED 16.16) + 4 × u16 colour (straight).
pub(in crate::kms::render::backend) fn parse_gradient_stops(
    body: &[u8],
    stops_offset: usize,
) -> Option<Vec<GradientStop>> {
    if body.len() < stops_offset + 4 {
        return None;
    }
    let n = u32::from_le_bytes(body[stops_offset..stops_offset + 4].try_into().ok()?) as usize;
    let pos_base = stops_offset + 4;
    let color_base = pos_base + n * 4;
    if body.len() < color_base + n * 8 {
        return None;
    }
    let mut stops: Vec<GradientStop> = Vec::with_capacity(n);
    for i in 0..n {
        let pos = i32::from_le_bytes(
            body[pos_base + i * 4..pos_base + i * 4 + 4]
                .try_into()
                .ok()?,
        );
        let cb = color_base + i * 8;
        let r = u16::from_le_bytes(body[cb..cb + 2].try_into().ok()?);
        let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().ok()?);
        let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().ok()?);
        let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().ok()?);
        stops.push(GradientStop { pos, r, g, b, a });
    }
    Some(stops)
}

/// Claim a one-shot: `true` exactly once per flag, for the caller that
/// got there first. Split out from its call site so the rate limiting
/// is testable without capturing log output, and without a test having
/// to consume a process-wide one-shot that another test may need.
pub(in crate::kms::render::backend) fn take_first_occurrence(
    flag: &std::sync::atomic::AtomicBool,
) -> bool {
    !flag.swap(true, std::sync::atomic::Ordering::Relaxed)
}

pub(in crate::kms::render::backend) fn change_picture_apply_mask(
    core: &mut KmsCore,
    host_pic: u32,
    body: &[u8],
) {
    if body.len() < 8 {
        return;
    }
    let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let values = &body[8..];
    let mut off = 0usize;
    let next_u32 = |off: &mut usize| -> Option<u32> {
        let bytes = values.get(*off..*off + 4)?;
        *off += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };
    for bit in 0..13 {
        let mask_bit = 1u32 << bit;
        if value_mask & mask_bit == 0 {
            continue;
        }
        let Some(v) = next_u32(&mut off) else {
            break;
        };
        match mask_bit {
            // CPRepeat
            0x0001 => {
                let repeat = match v {
                    1 => Repeat::Normal,
                    2 => Repeat::Pad,
                    3 => Repeat::Reflect,
                    _ => Repeat::None,
                };
                match core.pictures.get_mut(&host_pic) {
                    Some(PictureRecord::Drawable { repeat: r, .. })
                    | Some(PictureRecord::SolidFill { repeat: r, .. })
                    | Some(PictureRecord::LinearGradient { repeat: r, .. })
                    | Some(PictureRecord::RadialGradient { repeat: r, .. }) => *r = repeat,
                    None => {}
                }
            }
            // CPAlphaMap
            0x0002 => {
                if let Some(PictureRecord::Drawable { alpha_map, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_map = if v == 0 { None } else { Some(v) };
                }
            }
            // CPAlphaXOrigin
            0x0004 => {
                if let Some(PictureRecord::Drawable { alpha_x, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_x = v as i16;
                }
            }
            // CPAlphaYOrigin
            0x0008 => {
                if let Some(PictureRecord::Drawable { alpha_y, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_y = v as i16;
                }
            }
            // CPClipXOrigin
            0x0010 => {
                if let Some(PictureRecord::Drawable { clip, clip_x, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    let new_x = v as i16;
                    let dx = i32::from(new_x) - i32::from(*clip_x);
                    if dx != 0
                        && let Some(rects) = clip.as_mut()
                    {
                        for r in rects {
                            r.x = (i32::from(r.x) + dx).clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        }
                    }
                    *clip_x = new_x;
                }
            }
            // CPClipYOrigin
            0x0020 => {
                if let Some(PictureRecord::Drawable { clip, clip_y, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    let new_y = v as i16;
                    let dy = i32::from(new_y) - i32::from(*clip_y);
                    if dy != 0
                        && let Some(rects) = clip.as_mut()
                    {
                        for r in rects {
                            r.y = (i32::from(r.y) + dy).clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        }
                    }
                    *clip_y = new_y;
                }
            }
            // CPClipMask: a depth-1 pixmap xid (or `None` = 0).
            // For Stage 3b parity with v1, we don't synthesize the
            // pixmap → rect-list conversion (v1 needs the pixmap's
            // dimensions, which it had on KmsBackend.pixmaps). v2's
            // DrawableStore exposes the same dims via the storage's
            // extent, but for the common path (Cairo never sets a
            // bitmap mask via ChangePicture — it uses
            // SetPictureClipRectangles) this stays a logged no-op.
            // Risk-listed for the rendercheck clip-mask category.
            0x0040 => {
                if v == 0 {
                    if let Some(PictureRecord::Drawable { clip, .. }) =
                        core.pictures.get_mut(&host_pic)
                    {
                        *clip = None;
                    }
                } else {
                    log::debug!(
                        "render ChangePicture CPClipMask=pixmap {v:#x} on picture {host_pic:#x}: \
                         bitmap-mask clip not yet wired (Stage 3b TODO; rendercheck-only path)"
                    );
                }
            }
            // CPGraphicsExposure
            0x0080 => {
                if let Some(PictureRecord::Drawable {
                    graphics_exposure, ..
                }) = core.pictures.get_mut(&host_pic)
                {
                    *graphics_exposure = v != 0;
                }
            }
            // CPSubwindowMode
            0x0100 => {
                if let Some(PictureRecord::Drawable { subwindow_mode, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *subwindow_mode = v as u8;
                }
            }
            // CPPolyEdge
            0x0200 => {
                if let Some(PictureRecord::Drawable { poly_edge, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *poly_edge = v as u8;
                }
            }
            // CPPolyMode
            0x0400 => {
                if let Some(PictureRecord::Drawable { poly_mode, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *poly_mode = v as u8;
                }
            }
            // CPDither: consumed but intentionally not stored
            // (v1 same behaviour).
            0x0800 => {}
            // CPComponentAlpha
            0x1000 => match core.pictures.get_mut(&host_pic) {
                Some(PictureRecord::Drawable {
                    component_alpha, ..
                })
                | Some(PictureRecord::SolidFill {
                    component_alpha, ..
                }) => *component_alpha = v != 0,
                _ => {}
            },
            _ => {}
        }
    }
}

/// Stage 3f.13 glyph fallback: pull the first stop's premultiplied
/// RGBA from a gradient picture record. Returns `None` if `host_pic`
/// isn't a gradient or has zero stops. Used by `composite_glyphs`
/// when a gradient source needs a solid-fill approximation — the
/// glyph paint path only knows how to sample a single colour, so a
/// proper LUT-sampled gradient on glyphs would need a separate
/// pipeline (deferred past Stage 3).
pub(in crate::kms::render::backend) fn first_stop_premul_of_gradient(
    core: &KmsCore,
    host_pic: u32,
) -> Option<[f32; 4]> {
    let stop = match core.pictures.get(&host_pic)? {
        PictureRecord::LinearGradient { stops, .. }
        | PictureRecord::RadialGradient { stops, .. } => stops.first()?,
        _ => return None,
    };
    let a = f32::from(stop.a) / 65535.0;
    let r = (f32::from(stop.r) / 65535.0) * a;
    let g = (f32::from(stop.g) / 65535.0) * a;
    let b = (f32::from(stop.b) / 65535.0) * a;
    Some([r, g, b, a])
}

/// Stage 3c: dst picture resolution. RENDER paint ops require
/// the dst to be a `PictureRecord::Drawable` (you can't paint
/// into a SolidFill or a Gradient). Returns the underlying
/// dst drawable's `host_xid` plus the picture's clip rectangles
/// (already pre-shifted by `clip_x` / `clip_y` per Stage 3b).
///
/// Stage 4a: callers feed `host_xid` through
/// `KmsBackend::resolve_paint_target` to apply COMPOSITE
/// redirect routing. The free function stays pure
/// (`&KmsCore`-only) so it can also be called from contexts
/// where the windows / parent chain isn't relevant.
pub(in crate::kms::render::backend) fn resolve_dst_picture_for_render(
    core: &KmsCore,
    host_pic: u32,
) -> Option<(u32, Option<Vec<Rectangle16>>)> {
    let PictureRecord::Drawable { host_xid, clip, .. } = core.pictures.get(&host_pic)? else {
        return None;
    };
    Some((*host_xid, clip.clone()))
}

/// Audit #2 (2026-05-19) — extract a source / mask picture's
/// `clientClip` for `render_composite`'s composite-region
/// computation. The picture's clip rects are stored
/// pre-shifted by `clip_x` / `clip_y` (see
/// `render_set_picture_clip_rectangles`), so the returned list
/// is already in the picture's drawable-local coord space —
/// `compute_render_composite_clip` translates from there into
/// dst space via `(xDst - xSrc, yDst - ySrc)`.
///
/// Non-Drawable pictures (`SolidFill` / gradients) carry no
/// `clientClip` and return `None`. `host_pic == 0` (the
/// "no mask" sentinel `RenderComposite` uses) also returns `None`.
/// #133 step 3 (P4) — the `RepeatNone` SOURCE-DOMAIN clip for a
/// picture that wraps a BORDERED window.
///
/// With storage-inclusive borders the sampled storage is larger than
/// the picture's drawable, so the shader's `uv ∈ [0, 1]` domain check
/// (which is normalised by the IMAGE extent, and cannot be given a
/// sub-rect without a push constant the 128-byte
/// `maxPushConstantsSize` minimum has no room for) would let a
/// `RepeatNone` sample outside the window return a border-ring texel
/// instead of nothing. Expressing the domain as a dst-space clip is
/// Xorg's own shape for source-side restriction —
/// `miClipPictureSrc(pRegion, pSrc, xDst - xSrc, yDst - ySrc)`
/// (`render/mipict.c:353-356`) — and `compute_render_composite_clip`
/// already carries exactly that translation for the client clip.
///
/// Returns `Some([(0, 0, w, h)])` — the window's own extent, in the
/// picture's drawable-local space, ready to be intersected into the
/// source clip — only when it actually restricts the sampled storage
/// (`border_width > 0`) and only for the case a rect can express:
///
/// - `RepeatNone` only. `Normal`/`Pad`/`Reflect` wrap or clamp against
///   the sampled image instead of suppressing, so they keep sampling
///   the whole storage (a bordered window's ring included). Xorg's fb
///   path has the same shape and is looser still: its pixman image is
///   the whole containing pixmap (`fb/fbpict.c:293-296`), so an
///   unredirected window there repeats over the entire screen pixmap.
/// - No picture transform. A transformed source's domain does not
///   project to a rectangle in dst space; Xorg leaves that to
///   sample-time domain checking too.
///
/// `None` at `bw == 0`, so the clip list is byte-identical there.
pub(in crate::kms::render::backend) fn picture_source_domain_clip(
    store: &crate::kms::render::store::DrawableStore,
    source: &crate::kms::render::engine::ResolvedSource,
    repeat: Repeat,
    transform: Option<&PictTransform>,
) -> Option<Vec<Rectangle16>> {
    use crate::kms::render::engine::ResolvedSource;
    let ResolvedSource::Drawable(sd) = source else {
        return None;
    };
    // `domain` is the resolved handle's own logical extent — the single
    // source of truth, set by `resolve_picture_for_render`. `None` for
    // pixmaps (including COMPOSITE-named window pixmaps, whose border
    // IS part of the drawable).
    let domain = sd.domain()?;
    if !matches!(repeat, Repeat::None) || transform.is_some() {
        return None;
    }
    // Only when the domain actually restricts the sampled storage.
    // A `bw == 0` window's content IS its storage, so this returns
    // `None` and the clip list stays byte-identical there.
    let storage = store.get(sd.id())?.storage.extent;
    if sd.offset() == (0, 0) && domain.width >= storage.width && domain.height >= storage.height {
        return None;
    }
    Some(vec![Rectangle16 {
        x: 0,
        y: 0,
        width: u16::try_from(domain.width).unwrap_or(u16::MAX),
        height: u16::try_from(domain.height).unwrap_or(u16::MAX),
    }])
}

fn picture_client_clip(core: &KmsCore, host_pic: u32) -> Option<Vec<Rectangle16>> {
    if host_pic == 0 {
        return None;
    }
    match core.pictures.get(&host_pic)? {
        PictureRecord::Drawable { clip, .. } => clip.clone(),
        PictureRecord::SolidFill { .. }
        | PictureRecord::LinearGradient { .. }
        | PictureRecord::RadialGradient { .. } => None,
    }
}

/// Compose the effective composite-region clip for `render_composite`
/// per X RENDER spec (`miComputeCompositeRegion`,
/// `/home/jos/Projects/xserver/render/mipict.c:316-389`):
///
///   clip = dst_clip ∩ src_clip-translated-to-dst-space ∩ mask_clip-translated-to-dst-space
///
/// Each argument may be `None`, which is interpreted as "no clip on
/// this picture" (paint everywhere). If all three are `None`, the
/// function returns `None` — the engine then applies its own
/// full-extent default. If any is `Some`, the result is `Some` and
/// carries the intersection (possibly empty, which means "paint
/// nothing" per X RENDER spec — Xorg returns FALSE here and skips
/// the draw).
///
/// `src_translation` and `mask_translation` are `(xDst - xSrc,
/// yDst - ySrc)` and `(xDst - xMask, yDst - yMask)` respectively
/// (per Xorg's `miClipPictureSrc` call sites at `mipict.c:356,370`).
/// `mask_clip` should be `None` when no mask is used.
///
/// Pure / no Vulkan; tested below against hand-traced Xorg vectors.
/// #135 — should this Composite acquire a source snapshot at all?
///
/// The size check belongs HERE rather than being left to the zero-area early
/// return inside `render_composite_inner`. Acquisition moved into the wrapper
/// so the release could have a single site, which put it AHEAD of that early
/// return: a `width == 0` Composite with a root source then did a full
/// scanout readback and a full-screen scratch upload before returning
/// nothing. Caught by codex on review of that refactor.
pub(in crate::kms::render::backend) fn composite_needs_source_snapshot(
    width: u16,
    height: u16,
) -> bool {
    width != 0 && height != 0
}

pub(in crate::kms::render::backend) fn dst_picture_clip_by_children(
    core: &KmsCore,
    host_pic: u32,
) -> bool {
    match core.pictures.get(&host_pic) {
        Some(PictureRecord::Drawable { subwindow_mode, .. }) => *subwindow_mode == 0,
        _ => true,
    }
}

/// #214: shrink a RENDER Trapezoids/Triangles union bbox `(x0, y0, x1,
/// y1)` to the extents of the clip it composites through (target
/// coords, already within the dst extent). The composite is scissored
/// to that clip anyway, so the coverage mask only needs to exist there;
/// `None` when nothing is left.
pub(in crate::kms::render::backend) fn clip_trap_bbox_to_extents(
    bbox: (i32, i32, i32, i32),
    clip: &[Rectangle16],
) -> Option<(i32, i32, u32, u32)> {
    let mut ext: Option<(i32, i32, i32, i32)> = None;
    for r in clip.iter().filter(|r| r.width > 0 && r.height > 0) {
        let (x0, y0) = (i32::from(r.x), i32::from(r.y));
        let (x1, y1) = (x0 + i32::from(r.width), y0 + i32::from(r.height));
        ext = Some(match ext {
            None => (x0, y0, x1, y1),
            Some(e) => (e.0.min(x0), e.1.min(y0), e.2.max(x1), e.3.max(y1)),
        });
    }
    let e = ext?;
    let (x0, y0) = (bbox.0.max(e.0), bbox.1.max(e.1));
    let (x1, y1) = (bbox.2.min(e.2), bbox.3.min(e.3));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    Some((x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

pub(in crate::kms::render::backend) fn compute_render_composite_clip(
    dst_clip: Option<&[Rectangle16]>,
    src_clip: Option<&[Rectangle16]>,
    src_translation: (i32, i32),
    mask_clip: Option<&[Rectangle16]>,
    mask_translation: (i32, i32),
) -> Option<Vec<Rectangle16>> {
    let src_in_dst =
        src_clip.map(|c| translate_clip_rects(c, src_translation.0, src_translation.1));
    let mask_in_dst =
        mask_clip.map(|c| translate_clip_rects(c, mask_translation.0, mask_translation.1));
    // Start with whichever input is Some, then fold the remaining
    // Some-inputs via intersection. Order doesn't matter — list
    // intersection is associative & commutative.
    let mut acc: Option<Vec<Rectangle16>> = None;
    let mut fold = |next: Option<Vec<Rectangle16>>| match (acc.take(), next) {
        (None, None) => {}
        (None, Some(v)) => acc = Some(v),
        (Some(a), None) => acc = Some(a),
        (Some(a), Some(b)) => acc = Some(intersect_clip_lists(&a, &b)),
    };
    fold(dst_clip.map(<[Rectangle16]>::to_vec));
    fold(src_in_dst);
    fold(mask_in_dst);
    acc
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_render_ops_render_format_for_ynest_id(
        &self,
        ynest_fmt: u32,
    ) -> Option<u32> {
        if ynest_fmt == 0 {
            None
        } else {
            Some(ynest_fmt)
        }
    }

    // ── RENDER ──────────────────────────────────────────────────
    pub(in crate::kms::render::backend) fn backend_render_ops_render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Stage 3b: real picture record. Insert default
        // `PictureRecord::Drawable`, incref a PIXMAP backing in the
        // store (so a `free_pixmap` on the backing survives while this
        // picture wraps it — picture_record_drawable_refcount test),
        // then delegate to render_change_picture for the value-mask
        // body.
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.core.next_host_xid();
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::drawable_default(drawable_xid, ynest_format),
        );
        // A window Picture holds no store ref: every use resolves the window's current storage.
        if matches!(host_drawable, AnyHandle::Pixmap(_)) {
            if let Some(id) = self.store.lookup(drawable_xid) {
                self.store.incref(id);
                self.picture_drawable_ids.insert(picture_xid, id);
            } else {
                // Backing not materialized yet (GLX-TFP / Present /
                // DRI3 import). Defer the incref:
                // `apply_pending_picture_refs` pins the backing the
                // moment it materializes, so a later `free_pixmap`
                // can't reach refcount 0 and destroy the drawable out
                // from under this live Picture.
                self.pending_picture_drawable_refs
                    .insert(picture_xid, drawable_xid);
            }
        }
        if value_mask != 0 {
            // Recompose the body shape that render_change_picture
            // expects: picture(4) + value_mask(4) + values.
            let mut body = Vec::with_capacity(8 + values.len());
            body.extend_from_slice(&picture_xid.to_le_bytes());
            body.extend_from_slice(&value_mask.to_le_bytes());
            body.extend_from_slice(values);
            self.render_change_picture(None, picture_xid, &body)?;
        }
        Ok(PictureHandle::from_raw(picture_xid))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        change_picture_apply_mask(&mut self.core, host_pic, body);
        Ok(())
    }

    /// Audit #8 (2026-05-19) — store the drawable-space origin of
    /// the wrapped surface on the picture record. The protocol
    /// layer calls this right after `render_create_picture` with
    /// the parent-relative `(x, y)` of a window-backed drawable
    /// (process_request.rs:1153). Pre-fix v2 inherited the trait
    /// default no-op so `drawable_origin` stayed at the
    /// `drawable_default` `(0, 0)` — clips on CSD-frame-child
    /// pictures couldn't translate external region geometry into
    /// picture-local coords.
    ///
    /// Non-Drawable picture variants (SolidFill / Linear /
    /// Radial gradient) have no drawable to anchor — tolerated
    /// no-op so the caller doesn't need to discriminate at the
    /// call site.
    pub(in crate::kms::render::backend) fn backend_render_ops_set_picture_drawable_origin(
        &mut self,
        host_pic: u32,
        origin: (i16, i16),
    ) {
        if let Some(PictureRecord::Drawable {
            drawable_origin, ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            *drawable_origin = origin;
        }
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        // Drop the record; if it was a Drawable variant, decref the
        // backing drawable in the store. SolidFill / Gradient
        // variants have no backing drawable — they own only the
        // GPU-side state on RenderEngine.picture_paint (Stage 3c).
        let retained_drawable_id = self.picture_drawable_ids.remove(&host_pic);
        if let Some(record) = self.core.pictures.remove(&host_pic)
            && record.drawable_host_xid().is_some()
        {
            if let Some(id) = retained_drawable_id {
                self.store_decref_with_invalidate(id);
            } else {
                // A window Picture, or a backing that never
                // materialized — no store ref was ever taken; just drop
                // any deferred ref request.
                self.pending_picture_drawable_refs.remove(&host_pic);
            }
        }
        // Drop any GPU-side state cached for this picture. Stage
        // 3b never populates the map (no gradient LUT built yet),
        // so this is a HashMap::remove no-op today; Stage 3c lazy-
        // builds gradient picture state through the same key, and
        // this teardown hook becomes load-bearing once that lands.
        self.engine.picture_paint_remove(host_pic);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState};

        let format = match ynest_format {
            RENDER_FMT_A8 => GlyphSetFormat::A8,
            RENDER_FMT_A1 => GlyphSetFormat::A1,
            RENDER_FMT_ARGB32 => GlyphSetFormat::Argb32,
            _ => GlyphSetFormat::Other,
        };
        let id = self.core.next_host_xid();
        self.core.glyphsets.insert(
            id,
            GlyphSetState {
                format,
                glyphs: HashMap::new(),
            },
        );
        Ok(GlyphSetHandle::from_raw(id))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        // Host glyphset xids are never reused, but the atlas entries
        // would otherwise outlive the set until the next atlas reset.
        self.core.glyphsets.remove(&host_gs);
        self.engine.forget_glyphs(host_gs, None);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        // Reuses v1's parse_add_glyphs — purely CPU-side, operates
        // on the KmsCore.glyphsets entry. Atlas-side upload (the
        // Vk part) is Stage 3d's render_composite_glyphs path.
        let Some(gs) = self.core.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        // AddGlyphs over a live id replaces its image (Xorg's AddGlyph);
        // the atlas copy of the old one must go with it.
        let redefined: Vec<u32> = body_tail
            .get(..4)
            .map(|n| u32::from_le_bytes([n[0], n[1], n[2], n[3]]) as usize)
            .and_then(|n| body_tail.get(4..4 + n.checked_mul(4)?))
            .map(|ids| {
                ids.chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .filter(|id| gs.glyphs.contains_key(id))
                    .collect()
            })
            .unwrap_or_default();
        crate::kms::backend::parse_add_glyphs(gs, body_tail);
        if !redefined.is_empty() {
            self.engine.forget_glyphs(host_gs, Some(&redefined));
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        let Some(gs) = self.core.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        let ids: Vec<u32> = glyph_ids
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for id in &ids {
            gs.glyphs.remove(id);
        }
        // A freed id can be added again with a different image.
        self.engine.forget_glyphs(host_gs, Some(&ids));
        Ok(())
    }

    /// #135 — acquire the IncludeInferiors root snapshot, run the composite,
    /// then release the snapshot on the way out.
    ///
    /// The split exists so the release has exactly ONE site. The first version
    /// of this freed the scratch pixmap at each of the six exits of
    /// `render_composite_inner` by hand, which is a leak waiting for the next
    /// early return to be added — one screen of storage per composite, and
    /// nothing in-tree can catch it because the snapshot only materialises
    /// with a live scanout (codex flagged exactly this risk). Structure it out
    /// instead of testing for it.
    pub(in crate::kms::render::backend) fn backend_render_ops_render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        // A source or mask that is the destination would follow it onto
        // the inferiors; such a self-composite keeps to the window.
        let fanout = if host_src == host_dst || host_mask == host_dst {
            Vec::new()
        } else {
            self.include_inferiors_dst_fanout(host_dst)
        };
        if !fanout.is_empty() {
            self.dst_fanout_active = true;
            let mut painted = self.render_composite(
                _origin, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x,
                dst_y, width, height,
            );
            self.dst_fanout_active = false;
            for (window, (ox, oy), clip) in fanout {
                let (x, y) = (shift_i16(dst_x, -ox), shift_i16(dst_y, -oy));
                let more = self.with_dst_picture_on(host_dst, window, clip, |b| {
                    b.render_composite(
                        _origin, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y,
                        x, y, width, height,
                    )
                });
                if let (Ok(painted), Some(Ok(more))) = (painted.as_mut(), more) {
                    painted.extend(more.into_iter().map(|r| xfixes::RegionRect {
                        x: shift_i16(r.x, ox),
                        y: shift_i16(r.y, oy),
                        ..r
                    }));
                }
            }
            return painted;
        }
        // Taken BEFORE the composite, because the substitution replaces the
        // source drawable entirely — but only when the request can paint at
        // all, so a zero-area Composite stays as free as it was before the
        // acquisition was hoisted out here.
        let inferiors_snapshot = if composite_needs_source_snapshot(width, height) {
            self.source_inferiors_snapshot(host_src)
        } else {
            None
        };
        let result = self.render_composite_inner(
            inferiors_snapshot,
            op,
            host_src,
            host_mask,
            host_dst,
            src_x,
            src_y,
            mask_x,
            mask_y,
            dst_x,
            dst_y,
            width,
            height,
        );
        if let Some(xid) = inferiors_snapshot {
            let _ = self.free_pixmap(None, xid);
        }
        result
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_picture_includes_inferiors(
        &self,
        host_pic: u32,
    ) -> bool {
        !dst_picture_clip_by_children(&self.core, host_pic)
            && matches!(
                self.core.pictures.get(&host_pic),
                Some(PictureRecord::Drawable { .. })
            )
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let fanout = self.include_inferiors_dst_fanout(host_dst);
        if !fanout.is_empty() {
            self.dst_fanout_active = true;
            let result =
                self.render_fill_rectangles(_origin, host_dst, op, color, rects, x_off, y_off);
            self.dst_fanout_active = false;
            for (window, (ox, oy), clip) in fanout {
                let (x, y) = (shift_i16(x_off, -ox), shift_i16(y_off, -oy));
                self.with_dst_picture_on(host_dst, window, clip, |b| {
                    b.render_fill_rectangles(_origin, host_dst, op, color, rects, x, y)
                });
            }
            return result;
        }
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!(
                "render render_fill_rectangles gap: host_dst 0x{host_dst:x} not a Drawable picture"
            );
            return Ok(());
        };
        // Stage 4a — redirect routing for dst.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_fill_rectangles gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset());
        let dst_clip = self.narrow_dst_clip_to_shared_backing(dst_host_xid, &dst_target, dst_clip);
        let (paint_dx, paint_dy) = dst_target.offset();

        // X RENDER XRenderColor is wire-premultiplied (rendercheck
        // main.c:337-345); pass through unchanged.
        let color_premul = [
            f32::from(u16::from_le_bytes([color[0], color[1]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[2], color[3]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[4], color[5]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[6], color[7]])) / 65535.0,
        ];

        let mut decoded: Vec<crate::kms::vk::ops::render::CompositeRect> =
            Vec::with_capacity(rects.len() / 8);
        for chunk in rects.chunks_exact(8) {
            let rx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(x_off);
            let ry = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(y_off);
            let rw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let rh = u16::from_le_bytes([chunk[6], chunk[7]]);
            if rw == 0 || rh == 0 {
                continue;
            }
            decoded.push(crate::kms::vk::ops::render::CompositeRect {
                src_x: 0,
                src_y: 0,
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(rx) + paint_dx,
                dst_y: i32::from(ry) + paint_dy,
                width: u32::from(rw),
                height: u32::from(rh),
            });
        }
        if decoded.is_empty() {
            return Ok(());
        }

        let stats = self.engine.render_fill_rectangles(
            &mut self.store,
            &mut self.platform,
            op,
            color_premul,
            dst_target.dst(),
            &decoded,
            dst_clip.as_deref(),
        );
        self.sync_descriptor_pool_telemetry();
        let n_rects = u32::try_from(decoded.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderFill,
                    dst_target.backing_id(),
                    n_rects,
                    op,
                    SrcClass::Solid,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!(
                "render render_fill_rectangles: engine returned {e:?} on dst 0x{host_dst:x}"
            );
        }
        // Phase B.2 Task 15: render_fill_rectangles may open a frame;
        // drain any resulting close events into telemetry so the
        // per-second emit picks them up without stale lag. Mirrors the
        // B.1 drain at the composite_glyphs wrapper.
        self.drain_frame_builder_telemetry();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        use crate::kms::{render::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        // Wire layout: each trapezoid is 40 bytes (10 × i32 16.16
        // fixed-point). Mirrors v1's try_vk_render_trapezoids_path
        // decoder (kms/backend.rs:4286).
        if traps.is_empty() {
            return Ok(Vec::new());
        }
        let n_traps = traps.len() / 40;
        if n_traps == 0 {
            return Ok(Vec::new());
        }
        let mut decoded: Vec<vk_traps::Trapezoid> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let read_i32 = |o: usize| -> i32 {
                i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
            };
            decoded.push(vk_traps::Trapezoid {
                top: read_i32(0),
                bottom: read_i32(4),
                left_p1: (read_i32(8), read_i32(12)),
                left_p2: (read_i32(16), read_i32(20)),
                right_p1: (read_i32(24), read_i32(28)),
                right_p2: (read_i32(32), read_i32(36)),
            });
        }
        // Xorg's `fbTrapezoids` (fb/fbtrap.c:164-165) subtracts the
        // first trapezoid's `left.p1` from xSrc/ySrc before forwarding
        // to pixman. This anchors the src origin at the first trap's
        // top-left, regardless of where the trap is in dst space. For
        // GTK CSD shadows (which pass `xSrc=20 ySrc=-25` for the BR
        // corner with `traps[0].left.p1 = (20, -25)`), the subtraction
        // resolves to src=(0,0) → no out-of-bounds sampling. Without
        // it, REPEAT_NONE returns transparent for the OOB rows and the
        // corner shadow has an 8-row α=0 gap.
        // Captured pre-shift; the dx/dy fold below moves the live trap
        // coords into the redirect-target space, but the *adjustment*
        // is from the client-supplied geometry.
        let first_trap_left_p1_x = decoded[0].left_p1.0 >> 16;
        let first_trap_left_p1_y = decoded[0].left_p1.1 >> 16;
        // Resolve src + dst via the same helpers render_composite
        // uses. The trap path doesn't read GC clip — picture clip
        // (from dst) is what scopes the draw (plan §4).
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            self.resolve_picture_for_render(host_src)
        else {
            log::debug!("render render_trapezoids gap: src 0x{host_src:x} not resolvable");
            return Ok(Vec::new());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("render render_trapezoids gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(Vec::new());
        };
        // Stage 4a — redirect routing for dst. The fold of
        // `x_off`/`y_off` and the redirect offset (both in pixel
        // units) into a single fixed-point delta keeps the
        // 16.16-arithmetic single-pass.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_trapezoids gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(Vec::new());
        };
        let dx = (i32::from(x_off) + dst_target.offset().0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset().1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut decoded {
                t.top = t.top.wrapping_add(dy);
                t.bottom = t.bottom.wrapping_add(dy);
                t.left_p1.0 = t.left_p1.0.wrapping_add(dx);
                t.left_p1.1 = t.left_p1.1.wrapping_add(dy);
                t.left_p2.0 = t.left_p2.0.wrapping_add(dx);
                t.left_p2.1 = t.left_p2.1.wrapping_add(dy);
                t.right_p1.0 = t.right_p1.0.wrapping_add(dx);
                t.right_p1.1 = t.right_p1.1.wrapping_add(dy);
                t.right_p2.0 = t.right_p2.0.wrapping_add(dx);
                t.right_p2.1 = t.right_p2.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::trapezoid_bbox(&decoded) else {
            return Ok(Vec::new());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;
        let bbox_local = Rectangle16 {
            x: i16::try_from((bx - dst_target.offset().0).max(0)).unwrap_or(i16::MAX),
            y: i16::try_from((by - dst_target.offset().1).max(0)).unwrap_or(i16::MAX),
            width: u16::try_from(bw).unwrap_or(u16::MAX),
            height: u16::try_from(bh).unwrap_or(u16::MAX),
        };
        let clip_by_children = dst_picture_clip_by_children(&self.core, host_dst);
        let dst_local_extent = self.dst_local_extent(dst_host_xid, dst_target.backing_id());
        let cliplist_local = self.render_dst_cliplist_local(
            dst_host_xid,
            clip_by_children,
            dst_clip.as_deref(),
            dst_local_extent,
            bbox_local,
        );
        if cliplist_local.is_empty() {
            return Ok(Vec::new());
        }
        let dst_clip =
            Self::shift_dst_picture_clip(Some(cliplist_local.clone()), dst_target.offset());
        let Some((bx, by, bw, bh)) =
            clip_trap_bbox_to_extents((bx, by, bx1, by1), dst_clip.as_deref().unwrap_or(&[]))
        else {
            return Ok(Vec::new());
        };

        // Pack instance bytes (40 bytes per trap; no padding —
        // asserted by `const _:()` in trap_pipeline.rs).
        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TrapInstanceData>();
        let mut instance_bytes = vec![0u8; stride * decoded.len()];
        for (i, t) in decoded.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // render_composite. Trap/tri paint into an xRGB32 dst on
        // depth-32 storage must drive "no alpha target," and
        // xRGB32 sources must pin α=ONE on the sample view.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        // Source origin in src-pixel space. Two adjustments stacked:
        //   - subtract `(x_off + redirect_offset)` to undo the dx/dy
        //     fold applied to the trap coords above;
        //   - subtract `traps[0].left.p1.{x,y}` to mirror Xorg's
        //     `fbTrapezoids` pixman pre-step (fb/fbtrap.c:164-165) —
        //     anchors src @ (0,0) at the first trap's top-left.
        // The emit folds in bbox for the non-full-dst branch.
        let src_origin_x =
            i32::from(src_x) - (i32::from(x_off) + dst_target.offset().0) - first_trap_left_p1_x;
        let src_origin_y =
            i32::from(src_y) - (i32::from(y_off) + dst_target.offset().1) - first_trap_left_p1_y;
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.dst(),
            TrapPrimKind::Trapezoid,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                decoded.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_origin_x,
            src_origin_y,
            src_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let n_traps = u32::try_from(decoded.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderTraps,
                    dst_target.backing_id(),
                    n_traps,
                    op,
                    src_class,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("render render_trapezoids: engine returned {e:?}");
        }
        Ok(local_rects_to_region(cliplist_local))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        use crate::kms::{render::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        let read_point = |off: usize, chunk: &[u8]| -> (i32, i32) {
            let x =
                i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
            let y = i32::from_le_bytes([
                chunk[off + 4],
                chunk[off + 5],
                chunk[off + 6],
                chunk[off + 7],
            ]);
            (x, y)
        };
        let mut tris: Vec<vk_traps::Triangle> = match minor {
            11 => {
                if !primitives.len().is_multiple_of(24) {
                    return Ok(Vec::new());
                }
                primitives
                    .chunks_exact(24)
                    .map(|c| vk_traps::Triangle {
                        p1: read_point(0, c),
                        p2: read_point(8, c),
                        p3: read_point(16, c),
                    })
                    .collect()
            }
            12 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(Vec::new());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (0..pts.len() - 2)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[i],
                        p2: pts[i + 1],
                        p3: pts[i + 2],
                    })
                    .collect()
            }
            13 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(Vec::new());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (1..pts.len() - 1)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[0],
                        p2: pts[i],
                        p3: pts[i + 1],
                    })
                    .collect()
            }
            _ => return Ok(Vec::new()),
        };
        if tris.is_empty() {
            return Ok(Vec::new());
        }
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            self.resolve_picture_for_render(host_src)
        else {
            log::debug!("render render_triangles gap: src 0x{host_src:x} not resolvable");
            return Ok(Vec::new());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("render render_triangles gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(Vec::new());
        };
        // Stage 4a — redirect routing for dst; fold the redirect
        // offset into the same fixed-point delta as `x_off/y_off`.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "render render_triangles gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(Vec::new());
        };
        let dx = (i32::from(x_off) + dst_target.offset().0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset().1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut tris {
                t.p1.0 = t.p1.0.wrapping_add(dx);
                t.p1.1 = t.p1.1.wrapping_add(dy);
                t.p2.0 = t.p2.0.wrapping_add(dx);
                t.p2.1 = t.p2.1.wrapping_add(dy);
                t.p3.0 = t.p3.0.wrapping_add(dx);
                t.p3.1 = t.p3.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::triangle_bbox(&tris) else {
            return Ok(Vec::new());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;
        let bbox_local = Rectangle16 {
            x: i16::try_from((bx - dst_target.offset().0).max(0)).unwrap_or(i16::MAX),
            y: i16::try_from((by - dst_target.offset().1).max(0)).unwrap_or(i16::MAX),
            width: u16::try_from(bw).unwrap_or(u16::MAX),
            height: u16::try_from(bh).unwrap_or(u16::MAX),
        };
        let clip_by_children = dst_picture_clip_by_children(&self.core, host_dst);
        let dst_local_extent = self.dst_local_extent(dst_host_xid, dst_target.backing_id());
        let cliplist_local = self.render_dst_cliplist_local(
            dst_host_xid,
            clip_by_children,
            dst_clip.as_deref(),
            dst_local_extent,
            bbox_local,
        );
        if cliplist_local.is_empty() {
            return Ok(Vec::new());
        }
        let dst_clip =
            Self::shift_dst_picture_clip(Some(cliplist_local.clone()), dst_target.offset());
        let Some((bx, by, bw, bh)) =
            clip_trap_bbox_to_extents((bx, by, bx1, by1), dst_clip.as_deref().unwrap_or(&[]))
        else {
            return Ok(Vec::new());
        };

        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TriangleInstanceData>();
        let mut instance_bytes = vec![0u8; stride * tris.len()];
        for (i, t) in tris.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // the trapezoid path; see that call site for rationale.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        // Source origin shifted by the same delta the triangle coords
        // were (x_off + redirect offset). Also subtract the first
        // triangle's `p1.{x,y}` to mirror Xorg's `fbTriangles` pixman
        // pre-step (fb/fbtrap.c:179-180) — anchors src @ (0,0) at the
        // first triangle's p1 regardless of where it sits in dst space.
        let first_tri_p1_x = tris[0].p1.0 >> 16;
        let first_tri_p1_y = tris[0].p1.1 >> 16;
        let src_origin_x =
            i32::from(src_x) - (i32::from(x_off) + dst_target.offset().0) - first_tri_p1_x;
        let src_origin_y =
            i32::from(src_y) - (i32::from(y_off) + dst_target.offset().1) - first_tri_p1_y;
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.dst(),
            TrapPrimKind::Triangle,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                tris.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_origin_x,
            src_origin_y,
            src_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let n_tris = u32::try_from(tris.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderTris,
                    dst_target.backing_id(),
                    n_tris,
                    op,
                    src_class,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("render render_triangles: engine returned {e:?}");
        }
        Ok(local_rects_to_region(cliplist_local))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill: 16-bit-per-channel colour,
        // little-endian, already premultiplied on the wire (per
        // rendercheck main.c:337-345). Store the channels as f32
        // exactly as received — the pipeline samples them
        // unchanged. Layout: r[0..2] g[2..4] b[4..6] a[6..8].
        let r16 = u16::from_le_bytes([color[0], color[1]]);
        let g16 = u16::from_le_bytes([color[2], color[3]]);
        let b16 = u16::from_le_bytes([color[4], color[5]]);
        let a16 = u16::from_le_bytes([color[6], color[7]]);
        let premul = [
            f32::from(r16) / 65535.0,
            f32::from(g16) / 65535.0,
            f32::from(b16) / 65535.0,
            f32::from(a16) / 65535.0,
        ];
        let picture_xid = self.core.next_host_xid();
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::SolidFill {
                premul,
                repeat: Repeat::Normal,
                component_alpha: false,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4) +
        // n_stops(4) + n × stop_pos(4) + n × stop_color(8).
        // Caller passes only the request payload from offset 4 —
        // the first u32 is interpreted as p1.x (sliced at body[4..]).
        if body.len() < 24 {
            return Ok(None);
        }
        let p1x = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let p1y = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let p2x = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let p2y = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 20) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the LUT eagerly so the first
        // render_composite against this picture has it ready. The
        // record + the engine's GradientPicture have parallel
        // lifetimes — render_free_picture drops both. Build
        // failure (no Vk on test fixture, or allocation error) is
        // non-fatal: the record still lands; render_composite
        // logs a gap if it can't find the LUT. This keeps the
        // logic-test fixture (no live Vk) usable without forcing
        // every gradient-create test through lavapipe.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_linear_gradient(
            &mut self.platform,
            picture_xid,
            (p1x, p1y),
            (p2x, p2y),
            &engine_stops,
        ) {
            log::debug!(
                "render render_create_linear_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::LinearGradient {
                p1: (p1x, p1y),
                p2: (p2x, p2y),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: icx(4) icy(4) ocx(4) ocy(4) ir(4) or(4)
        // n_stops(4) + stops + colors. Same offset-by-4 convention
        // as linear (first u32 in `body` is past the request header).
        if body.len() < 32 {
            return Ok(None);
        }
        let icx = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let icy = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let ocx = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let ocy = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let ir = i32::from_le_bytes(body[20..24].try_into().unwrap());
        let or_ = i32::from_le_bytes(body[24..28].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 28) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the radial LUT (256×256 BGRA) eagerly.
        // See `render_create_linear_gradient` for failure-mode
        // rationale.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_radial_gradient(
            &mut self.platform,
            picture_xid,
            (icx, icy, ir),
            (ocx, ocy, or_),
            &engine_stops,
        ) {
            log::debug!(
                "render render_create_radial_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::RadialGradient {
                inner: (icx, icy, ir),
                outer: (ocx, ocy, or_),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + name_len(u16) + pad(2) + name +
        // pad + N × FIXED(4) parameters. Stage 3 only honours
        // `nearest`; other filters parse + store so the record-
        // round-trip is honest but `RenderEngine` ignores them at
        // draw time (per Risk 6).
        if body.len() < 8 {
            return Ok(());
        }
        let name_len = u16::from_le_bytes([body[4], body[5]]) as usize;
        if body.len() < 8 + name_len {
            return Ok(());
        }
        let name = &body[8..8 + name_len];
        let filter = match name {
            b"nearest" | b"fast" => PictureFilter::Nearest,
            b"bilinear" | b"good" | b"best" => PictureFilter::Bilinear,
            b"convolution" => PictureFilter::Convolution,
            _ => PictureFilter::Nearest,
        };
        if let Some(PictureRecord::Drawable { filter: f, .. }) =
            self.core.pictures.get_mut(&host_pic)
        {
            *f = filter;
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_render_ops_render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + 9 × FIXED(4) matrix entries (row-
        // major). 16.16 fixed-point; identity is [[1,0,0],[0,1,0],
        // [0,0,1]] in floating shape, [[0x10000, 0, 0], [0, 0x10000,
        // 0], [0, 0, 0x10000]] in fixed.
        if body.len() < 40 {
            return Ok(());
        }
        let mut matrix = [[0i32; 3]; 3];
        for (idx, slot) in matrix.iter_mut().flatten().enumerate() {
            let off = 4 + idx * 4;
            *slot = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        }
        let transform = if matrix == [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]] {
            None
        } else {
            Some(PictTransform { matrix })
        };
        match self.core.pictures.get_mut(&host_pic) {
            Some(PictureRecord::Drawable { transform: t, .. })
            | Some(PictureRecord::LinearGradient { transform: t, .. })
            | Some(PictureRecord::RadialGradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }
}
