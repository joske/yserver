use super::*;

impl KmsBackend {
    /// Compute the destination window's RENDER clipList for one paint op
    /// in destination-window-local coordinates.
    pub(in crate::kms::render::backend) fn render_dst_cliplist_local(
        &self,
        dst_host_xid: u32,
        clip_by_children: bool,
        pre_shift_picture_clip: Option<&[Rectangle16]>,
        dst_local_extent: Rectangle16,
        op_bbox_local: Rectangle16,
    ) -> Vec<Rectangle16> {
        let extent = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(dst_local_extent.x),
                y: i32::from(dst_local_extent.y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(dst_local_extent.width),
                height: u32::from(dst_local_extent.height),
            },
        };
        let base: Vec<ash::vk::Rect2D> = match pre_shift_picture_clip {
            Some(clip) => {
                let clip_rects: Vec<ash::vk::Rect2D> = clip
                    .iter()
                    .filter(|r| r.width > 0 && r.height > 0)
                    .map(|r| ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: i32::from(r.x),
                            y: i32::from(r.y),
                        },
                        extent: ash::vk::Extent2D {
                            width: u32::from(r.width),
                            height: u32::from(r.height),
                        },
                    })
                    .collect();
                intersect_rect_with_clip(extent, &clip_rects)
            }
            None => {
                if extent.extent.width == 0 || extent.extent.height == 0 {
                    Vec::new()
                } else {
                    vec![extent]
                }
            }
        };
        if base.is_empty() {
            return Vec::new();
        }
        let after_children: Vec<ash::vk::Rect2D> =
            if clip_by_children && self.windows.contains_key(&dst_host_xid) {
                let child_rects: Vec<ash::vk::Rect2D> = self
                    .windows
                    .iter()
                    .filter_map(|(child_host_xid, geom)| {
                        if !(geom.parent == Some(dst_host_xid) && geom.mapped) {
                            return None;
                        }
                        let is_manually_redirected = self
                            .store
                            .lookup(*child_host_xid)
                            .and_then(|id| self.store.get(id))
                            .is_some_and(|d| !d.scene_participating);
                        if is_manually_redirected {
                            return None;
                        }
                        // #133 step 3 round 5 — child rects live in the
                        // parent's CONTENT space: `(x + bw, y + bw)`.
                        // Same rule as the `IncludeInferiors` fan-out and
                        // `clip_fill_rects_by_subwindow_mode`; identity
                        // at `bw == 0`.
                        let child_bw = i32::from(geom.border_width);
                        let content_box = ash::vk::Rect2D {
                            offset: ash::vk::Offset2D {
                                x: i32::from(geom.x) + child_bw,
                                y: i32::from(geom.y) + child_bw,
                            },
                            extent: ash::vk::Extent2D {
                                width: u32::from(geom.width.max(1)),
                                height: u32::from(geom.height.max(1)),
                            },
                        };
                        Some(self.child_clip_region(*child_host_xid, geom, content_box))
                    })
                    .flatten()
                    .collect();
                if child_rects.is_empty() {
                    base
                } else {
                    base.into_iter()
                        .flat_map(|r| compute_copy_area_dst_rects(r, &child_rects))
                        .collect()
                }
            } else {
                base
            };
        let after_children = match self
            .resolve_paint_target(dst_host_xid)
            .and_then(|t| self.shared_backing_draw_clip(dst_host_xid, &t))
        {
            Some(keep) => after_children
                .into_iter()
                .flat_map(|r| intersect_rect_with_clip(r, &keep))
                .collect(),
            None => after_children,
        };
        if after_children.is_empty() {
            return Vec::new();
        }
        let bbox = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(op_bbox_local.x),
                y: i32::from(op_bbox_local.y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(op_bbox_local.width),
                height: u32::from(op_bbox_local.height),
            },
        };
        intersect_rect_with_clip(bbox, &after_children)
            .into_iter()
            .filter_map(|r| {
                Some(Rectangle16 {
                    x: i16::try_from(r.offset.x).ok()?,
                    y: i16::try_from(r.offset.y).ok()?,
                    width: u16::try_from(r.extent.width).ok()?,
                    height: u16::try_from(r.extent.height).ok()?,
                })
            })
            .collect()
    }

    /// The destination drawable's own extent in local coordinates.
    pub(in crate::kms::render::backend) fn dst_local_extent(
        &self,
        dst_host_xid: u32,
        dst_id: DrawableId,
    ) -> Rectangle16 {
        if let Some(g) = self.windows.get(&dst_host_xid) {
            Rectangle16 {
                x: 0,
                y: 0,
                width: g.width,
                height: g.height,
            }
        } else {
            let ext =
                self.store
                    .get(dst_id)
                    .map(|d| d.storage.extent)
                    .unwrap_or(ash::vk::Extent2D {
                        width: 0,
                        height: 0,
                    });
            Rectangle16 {
                x: 0,
                y: 0,
                width: u16::try_from(ext.width).unwrap_or(u16::MAX),
                height: u16::try_from(ext.height).unwrap_or(u16::MAX),
            }
        }
    }

    /// What a window's inferiors add to the pixmap it draws into, which
    /// Xorg's GetImage and RENDER source reads see (`DoGetImage`,
    /// `dix/dispatch.c:2176-2189`): the screen's or a redirected
    /// ancestor's, where its children have drawn over it. Here a window
    /// keeps its own storage unless it shares a redirected ancestor's,
    /// so list every viewable child, bottom to top, that does not draw
    /// into `parent_backing` already, with its paint target, its content
    /// origin and the pieces of it inside its bounding shape and its
    /// ancestors' rects (`clip`), all in the content space `parent_origin`
    /// is in; then its own such children. A Manual-redirected child is
    /// not drawn into its parent (`TreatAsTransparent`,
    /// `mi/mivaltree.c:171`).
    pub(in crate::kms::render::backend) fn inferior_pieces(
        &self,
        parent: u32,
        parent_origin: (i32, i32),
        clip: &[ash::vk::Rect2D],
        parent_backing: DrawableId,
        out: &mut Vec<InferiorPiece>,
    ) {
        let mut children: Vec<(u32, WindowGeometry)> = self
            .windows
            .iter()
            .filter(|(_, g)| g.parent == Some(parent) && g.mapped && g.viewable)
            .map(|(xid, g)| (*xid, *g))
            .collect();
        children.sort_by_key(|(_, g)| g.stack_rank);
        for (child, g) in children {
            let participating = self
                .store
                .lookup(child)
                .and_then(|id| self.store.get(id))
                .is_some_and(|d| d.scene_participating);
            if !participating {
                continue;
            }
            let Some(target) = self.resolve_paint_target(child) else {
                continue;
            };
            let bw = i32::from(g.border_width);
            let outer = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(g.x),
                    y: i32::from(g.y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(g.width) + 2 * u32::from(g.border_width),
                    height: u32::from(g.height) + 2 * u32::from(g.border_width),
                },
            };
            let visible: Vec<ash::vk::Rect2D> = self
                .child_clip_region(child, &g, outer)
                .into_iter()
                .map(|r| ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: r.offset.x + parent_origin.0,
                        y: r.offset.y + parent_origin.1,
                    },
                    extent: r.extent,
                })
                .flat_map(|r| intersect_rect_with_clip(r, clip))
                .collect();
            if visible.is_empty() {
                continue;
            }
            let origin = (
                parent_origin.0 + i32::from(g.x) + bw,
                parent_origin.1 + i32::from(g.y) + bw,
            );
            let content = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: origin.0,
                    y: origin.1,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(g.width),
                    height: u32::from(g.height),
                },
            };
            let inner: Vec<ash::vk::Rect2D> = visible
                .iter()
                .flat_map(|r| intersect_rect_with_clip(*r, &[content]))
                .collect();
            if target.backing_id() != parent_backing {
                out.push(InferiorPiece {
                    window: child,
                    target,
                    origin,
                    rects: visible,
                });
            }
            if !inner.is_empty() {
                self.inferior_pieces(child, origin, &inner, target.backing_id(), out);
            }
        }
    }

    /// Paste [`Self::inferior_pieces`] of `host` over `buf`, its `area`
    /// read in `host`'s content space (4-byte pixels).
    pub(in crate::kms::render::backend) fn paste_inferiors(
        &mut self,
        host: u32,
        backing: DrawableId,
        area: ash::vk::Rect2D,
        depth: u8,
        buf: &mut [u8],
    ) {
        let mut pieces = Vec::new();
        self.inferior_pieces(host, (0, 0), &[area], backing, &mut pieces);
        for piece in pieces {
            let Some(bbox) = vk_rects_bbox(&piece.rects) else {
                continue;
            };
            let storage = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: bbox.offset.x - piece.origin.0 + piece.target.offset().0,
                    y: bbox.offset.y - piece.origin.1 + piece.target.offset().1,
                },
                extent: bbox.extent,
            };
            match self.engine.get_image(
                &mut self.store,
                &mut self.platform,
                piece.target.src_including_border(),
                storage,
                depth,
            ) {
                Ok(bytes) => {
                    for r in &piece.rects {
                        blit_rows_4bpp(&bytes, bbox, buf, area, *r);
                    }
                }
                Err(e) => {
                    log::debug!(
                        "render get_image: inferior {:#x} readback: {e:?}",
                        piece.window
                    );
                }
            }
        }
    }

    /// IncludeInferiors on a destination Picture whose window draws into
    /// storage of its own: Xorg clips the op to the window's borderClip
    /// in the pixmap it shares with its inferiors (`miValidatePicture`,
    /// `render/mipict.c:114-118`), so it lands on them too. Each such
    /// inferior with storage of its own ([`Self::inferior_pieces`]), its
    /// content origin in the window's space, and the clip it takes the op
    /// through, in its own space. Empty for every other picture.
    pub(in crate::kms::render::backend) fn include_inferiors_dst_fanout(
        &self,
        host_pic: u32,
    ) -> Vec<(u32, (i32, i32), Vec<Rectangle16>)> {
        if self.dst_fanout_active {
            return Vec::new();
        }
        let Some(PictureRecord::Drawable {
            host_xid,
            clip,
            subwindow_mode: 1,
            ..
        }) = self.core.pictures.get(&host_pic)
        else {
            return Vec::new();
        };
        let Some(geom) = self.windows.get(host_xid) else {
            return Vec::new();
        };
        let Some(target) = self.resolve_paint_target(*host_xid) else {
            return Vec::new();
        };
        let content = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: u32::from(geom.width),
                height: u32::from(geom.height),
            },
        };
        let base: Vec<vk::Rect2D> = match clip {
            Some(rects) => rects
                .iter()
                .filter(|r| r.width > 0 && r.height > 0)
                .flat_map(|r| {
                    intersect_rect_with_clip(
                        vk::Rect2D {
                            offset: vk::Offset2D {
                                x: i32::from(r.x),
                                y: i32::from(r.y),
                            },
                            extent: vk::Extent2D {
                                width: u32::from(r.width),
                                height: u32::from(r.height),
                            },
                        },
                        &[content],
                    )
                })
                .collect(),
            None => vec![content],
        };
        let mut pieces = Vec::new();
        self.inferior_pieces(*host_xid, (0, 0), &base, target.backing_id(), &mut pieces);
        pieces
            .into_iter()
            .map(|p| {
                let local: Vec<vk::Rect2D> = p
                    .rects
                    .iter()
                    .map(|r| vk::Rect2D {
                        offset: vk::Offset2D {
                            x: r.offset.x - p.origin.0,
                            y: r.offset.y - p.origin.1,
                        },
                        extent: r.extent,
                    })
                    .collect();
                let unclipped = [Rectangle16 {
                    x: i16::MIN,
                    y: i16::MIN,
                    width: u16::MAX,
                    height: u16::MAX,
                }];
                (p.window, p.origin, clip_rects16(&unclipped, &local))
            })
            .collect()
    }

    /// Run `op` with Picture `host_pic` pointing at `window` through
    /// `clip` (both in `window`'s space), then put the picture back.
    pub(in crate::kms::render::backend) fn with_dst_picture_on<R>(
        &mut self,
        host_pic: u32,
        window: u32,
        clip: Vec<Rectangle16>,
        op: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        let saved = self.core.pictures.get(&host_pic).cloned()?;
        if let Some(PictureRecord::Drawable {
            host_xid, clip: c, ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            *host_xid = window;
            *c = Some(clip);
        }
        self.dst_fanout_active = true;
        let r = op(self);
        self.dst_fanout_active = false;
        self.core.pictures.insert(host_pic, saved);
        Some(r)
    }

    pub(in crate::kms::render::backend) fn collect_fill_rects_for_inferiors(
        &self,
        host_xid: u32,
        rects: &[Rectangle16],
    ) -> Vec<(u32, Vec<Rectangle16>)> {
        fn walk(
            backend: &KmsBackend,
            parent_xid: u32,
            rects: &[Rectangle16],
            out: &mut Vec<(u32, Vec<Rectangle16>)>,
        ) {
            for (child_xid, geom) in &backend.windows {
                let is_child = if parent_xid == backend.core.window_id {
                    geom.parent == Some(backend.core.window_id) || geom.parent.is_none()
                } else {
                    geom.parent == Some(parent_xid)
                };
                if !is_child || !geom.mapped {
                    continue;
                }
                // #133 step 3 round 5 — the child's rect in the
                // PARENT'S CONTENT space is `(x + bw, y + bw, w, h)`:
                // `x`/`y` are the child's OUTER origin, and its own
                // drawing coordinates start at its CONTENT origin, `bw`
                // further in (`dix/window.c` sets
                // `drawable.x = parent->drawable.x + x + bw`). Both the
                // intersection AND the translation must use that, or a
                // fanned-out `IncludeInferiors` draw lands `bw` px away
                // from where the identical draw performed directly on
                // the child lands — which is exactly what the child's
                // paint target then adds back. `(0, 0)` at `bw == 0`, so
                // this is the pre-#133 arithmetic there.
                let child_bw = i32::from(geom.border_width);
                let child_x = i32::from(geom.x) + child_bw;
                let child_y = i32::from(geom.y) + child_bw;
                let child_w = i32::from(geom.width);
                let child_h = i32::from(geom.height);
                let mut child_rects = Vec::new();
                for r in rects {
                    let rx0 = i32::from(r.x);
                    let ry0 = i32::from(r.y);
                    let rx1 = rx0 + i32::from(r.width);
                    let ry1 = ry0 + i32::from(r.height);
                    let ix0 = rx0.max(child_x);
                    let iy0 = ry0.max(child_y);
                    let ix1 = rx1.min(child_x + child_w);
                    let iy1 = ry1.min(child_y + child_h);
                    if ix0 < ix1 && iy0 < iy1 {
                        child_rects.push(Rectangle16 {
                            x: (ix0 - child_x) as i16,
                            y: (iy0 - child_y) as i16,
                            width: (ix1 - ix0) as u16,
                            height: (iy1 - iy0) as u16,
                        });
                    }
                }
                if child_rects.is_empty() {
                    continue;
                }
                out.push((*child_xid, child_rects.clone()));
                walk(backend, *child_xid, &child_rects, out);
            }
        }

        let mut out = Vec::new();
        walk(self, host_xid, rects, &mut out);
        out
    }

    /// XOR-safe inferior collection for `IncludeInferiors` stroke painting.
    ///
    /// Unlike `collect_fill_rects_for_inferiors` (which recurses into every
    /// mapped descendant and can double-cover a backing — harmless under
    /// idempotent `GXcopy` fills, but under `GXinvert` a second pass over the
    /// same backing pixels CANCELS the first), this resolves each contributing
    /// window to its `PaintTarget` and emits each distinct backing at most
    /// once. A descendant that routes into a backing already covered by an
    /// ancestor is skipped (the ancestor's entry covers those pixels); the walk
    /// still recurses to find independently-redirected descendants (distinct
    /// backings). Non-`scene_participating` (manually-redirected) windows are
    /// skipped entirely, matching `clip_fill_rects_by_subwindow_mode`.
    ///
    /// `rects` and the returned rects are in each window's LOCAL coordinates;
    /// `fill_solid_rects` applies the `PaintTarget.offset` into the backing.
    pub(in crate::kms::render::backend) fn collect_stroke_inferior_targets(
        &self,
        host_xid: u32,
        rects: &[Rectangle16],
    ) -> Vec<(PaintTarget, Vec<Rectangle16>)> {
        use std::collections::HashSet;
        let mut seen: HashSet<crate::kms::render::store::DrawableId> = HashSet::new();
        // Seed with the host's own target so an inferior routing into the host
        // backing (e.g. a top-level when root itself is redirected) is skipped.
        if let Some(t) = self.resolve_paint_target(host_xid) {
            seen.insert(t.backing_id());
        }
        let mut out = Vec::new();
        self.walk_stroke_inferiors(host_xid, rects, &mut seen, &mut out);
        out
    }

    fn walk_stroke_inferiors(
        &self,
        parent_xid: u32,
        rects: &[Rectangle16],
        seen: &mut std::collections::HashSet<crate::kms::render::store::DrawableId>,
        out: &mut Vec<(PaintTarget, Vec<Rectangle16>)>,
    ) {
        for (child_xid, geom) in &self.windows {
            let is_child = if parent_xid == self.core.window_id {
                geom.parent == Some(self.core.window_id) || geom.parent.is_none()
            } else {
                geom.parent == Some(parent_xid)
            };
            if !is_child || !geom.mapped {
                continue;
            }
            // Skip manually-redirected (non-participating) windows; their
            // backing is composited separately and not part of this draw.
            let participating = self
                .store
                .lookup(*child_xid)
                .and_then(|id| self.store.get(id))
                .is_some_and(|d| d.scene_participating);
            if !participating {
                continue;
            }
            // Intersect the parent-local rects with this child's geometry and
            // translate into child-local coords (same math as
            // `collect_fill_rects_for_inferiors`) — including the
            // `+ bw` that puts the child's rect in the parent's CONTENT
            // space and makes the translation land on the child's own
            // content origin (#133 step 3 round 5).
            let cbw = i32::from(geom.border_width);
            let cx = i32::from(geom.x) + cbw;
            let cy = i32::from(geom.y) + cbw;
            let cw = i32::from(geom.width);
            let ch = i32::from(geom.height);
            let mut child_rects = Vec::new();
            for r in rects {
                let rx0 = i32::from(r.x);
                let ry0 = i32::from(r.y);
                let rx1 = rx0 + i32::from(r.width);
                let ry1 = ry0 + i32::from(r.height);
                let ix0 = rx0.max(cx);
                let iy0 = ry0.max(cy);
                let ix1 = rx1.min(cx + cw);
                let iy1 = ry1.min(cy + ch);
                if ix0 < ix1 && iy0 < iy1 {
                    child_rects.push(Rectangle16 {
                        x: (ix0 - cx) as i16,
                        y: (iy0 - cy) as i16,
                        width: (ix1 - ix0) as u16,
                        height: (iy1 - iy0) as u16,
                    });
                }
            }
            if child_rects.is_empty() {
                continue;
            }
            let Some(target) = self.resolve_paint_target(*child_xid) else {
                continue;
            };
            // Emit only when this child introduces a NEW backing; either way,
            // recurse to discover independently-redirected descendants.
            if seen.insert(target.backing_id()) {
                out.push((target, child_rects.clone()));
            }
            self.walk_stroke_inferiors(*child_xid, &child_rects, seen, out);
        }
    }

    /// [`fill_rects_honoring_fill_state`] for the Solid arm.
    ///
    /// `GcFunction::Copy` (the common case) goes through the fast
    /// `vkCmdClearAttachments`-driven `engine.fill_rect`. Non-`Copy`
    /// functions (Stage 3f.2: GXclear / GXxor / GXinvert / etc.)
    /// divert to `engine.logic_fill`, which builds a per-function
    /// `VkLogicOp` pipeline through the shared
    /// `LogicFillPipelineCache`. `GcFunction::NoOp` is a no-op.
    ///
    /// Stage 4a: `target` carries the resolved DrawableId + a
    /// paint-translation offset for COMPOSITE redirect. Window-
    /// local `rects` are shifted by `target.offset()` before going
    /// to the engine.
    /// Build the per-call stroke snapshot from the GC state captured
    /// in `apply_draw_state`.
    pub(in crate::kms::render::backend) fn current_stroke_state(
        &self,
        _foreground: u32,
    ) -> crate::kms::render::stroke::StrokeState {
        crate::kms::render::stroke::StrokeState {
            background: self.core.current_background,
            line_width: self.core.current_line_width,
            line_style: self.core.current_line_style,
            cap_style: self.core.current_cap_style,
            join_style: self.core.current_join_style,
            dashes: self.core.current_dashes.clone(),
            dash_offset: self.core.current_dash_offset,
        }
    }

    /// Clip a stroke's fg/bg rect lists against the current GC clip
    /// and submit them. `LineStyle::DoubleDash` off-runs land in
    /// `bg_rects` and paint in the GC background colour.
    /// True iff this draw is a reversible-logic (`GXinvert`/`GXxor`) op to the
    /// ROOT window with `IncludeInferiors`. Gate BEFORE looking at rects so a
    /// background-only run (e.g. `LineStyle::DoubleDash`) still skips backing.
    pub(in crate::kms::render::backend) fn is_root_overlay_draw(&self, host_xid: u32) -> bool {
        use yserver_core::backend::{GcFunction, SubwindowMode};
        host_xid == self.core.window_id
            && matches!(
                self.core.current_subwindow_mode,
                SubwindowMode::IncludeInferiors
            )
            && matches!(
                self.core.current_function,
                GcFunction::Invert | GcFunction::Xor
            )
    }

    /// Route this draw to the front-buffer overlay instead of root's backing?
    /// Requires a real client origin — a None-origin internal draw (ClearArea
    /// background clear, put_image decomposition) must still paint the backing,
    /// and must not be swallowed by stale GC scratch state.
    pub(in crate::kms::render::backend) fn should_route_root_overlay(
        &self,
        host_xid: u32,
        origin: Option<OriginContext>,
    ) -> bool {
        origin.is_some() && self.is_root_overlay_draw(host_xid)
    }

    /// Fold one color-run's rects into the front-buffer overlay. Assumes
    /// `is_root_overlay_draw(host_xid)` already held. `color` is the run's fill
    /// value (fg for solid/fg runs, GC background for DoubleDash off-runs).
    pub(in crate::kms::render::backend) fn capture_root_overlay(
        &mut self,
        origin: Option<OriginContext>,
        color: u32,
        rects: &[Rectangle16],
    ) {
        if rects.is_empty() {
            return;
        }
        let depth = self
            .store
            .lookup(self.core.window_id)
            .and_then(|id| self.store.get(id))
            .map_or(24, |d| d.depth);
        let plane_mask = self.core.current_plane_mask & depth_plane_mask(depth);
        if plane_mask == 0 {
            return;
        }
        let Some(value) = crate::kms::render::root_overlay::xor_value_for(
            self.core.current_function,
            color,
            plane_mask,
        ) else {
            return;
        };
        let Some(client) = origin.map(|o| o.client_id) else {
            return;
        };
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter(|r| r.width != 0 && r.height != 0)
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(r.x),
                    y: i32::from(r.y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            })
            .collect();
        if vk_rects.is_empty() {
            return;
        }
        self.scene.root_overlay_toggle(client, value, &vk_rects);
    }

    pub(in crate::kms::render::backend) fn emit_stroke_output(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        target: PaintTarget,
        foreground: u32,
        background: u32,
        out: crate::kms::render::stroke::StrokeOutput,
    ) {
        let include_inferiors = matches!(
            self.core.current_subwindow_mode,
            yserver_core::backend::SubwindowMode::IncludeInferiors,
        ) && (self.windows.contains_key(&host_xid)
            || host_xid == self.core.window_id);

        // Apply the GC clip FIRST, then collect inferiors from the clipped rects.
        // The clip-mask applies to drawing into inferiors too (X11 semantics), and
        // this matches the fill path where callers pre-clip before
        // `fill_rects_honoring_fill_state` collects inferiors.
        let fg_clipped = self.intersect_with_current_clip_live(&out.fg_rects);
        let bg_clipped = self.intersect_with_current_clip_live(&out.bg_rects);

        // Legacy root-overlay idiom: reroute reversible root+IncludeInferiors
        // strokes to the compose-time front-buffer overlay instead of the
        // (occluded) root backing. BOTH fg and bg runs toggle; gate on the
        // op-kind predicate (not rect emptiness) so a bg-only DoubleDash run
        // still skips backing.
        if self.should_route_root_overlay(host_xid, origin) {
            self.capture_root_overlay(origin, foreground, &fg_clipped);
            self.capture_root_overlay(origin, background, &bg_clipped);
            return;
        }

        let fg_inferiors = if include_inferiors {
            self.collect_stroke_inferior_targets(host_xid, &fg_clipped)
        } else {
            Vec::new()
        };
        let bg_inferiors = if include_inferiors {
            self.collect_stroke_inferior_targets(host_xid, &bg_clipped)
        } else {
            Vec::new()
        };

        // Host's own backing, through its clip list: its children out
        // under ClipByChildren, and what of a backing it shares with its
        // ancestors it may not paint — the same clip every fill takes.
        let clip = if fg_clipped.is_empty() && bg_clipped.is_empty() {
            None
        } else {
            self.subwindow_mode_clip(host_xid, self.core.current_subwindow_mode)
        };
        let own = |rects: &[Rectangle16]| match &clip {
            Some(clip) if !rects.is_empty() => apply_subwindow_mode_clip(clip, rects),
            _ => rects.to_vec(),
        };
        let (fg_own, bg_own) = (own(&fg_clipped), own(&bg_clipped));
        if !fg_own.is_empty() {
            self.fill_solid_rects(target, foreground, &fg_own);
        }
        if !bg_own.is_empty() {
            self.fill_solid_rects(target, background, &bg_own);
        }

        // Each distinct inferior backing, exactly once (XOR-safe).
        for (child_target, child_rects) in fg_inferiors {
            self.fill_solid_rects(child_target, foreground, &child_rects);
        }
        for (child_target, child_rects) in bg_inferiors {
            self.fill_solid_rects(child_target, background, &child_rects);
        }
    }
}
