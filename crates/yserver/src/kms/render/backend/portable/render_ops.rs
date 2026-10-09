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
