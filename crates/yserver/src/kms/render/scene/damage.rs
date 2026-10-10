use super::*;

impl SceneCompositor {
    pub(super) fn note_structure_change(&mut self) {
        self.scene_structure_dirty = true;
        self.structure_generation = self.structure_generation.wrapping_add(1);
    }

    /// Mark the scene as needing a redraw. Cheap bool flip;
    /// callable from any mutation path that wants the next tick
    /// to inspect drawable/cursor damage. This deliberately does
    /// NOT add output damage: protocol paint is already represented
    /// by per-drawable presentation damage, and cursor motion is
    /// projected by `build_scene`.
    #[track_caller]
    pub(crate) fn wake_for_damage(&mut self) {
        if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), self.full_output_audit_area());
        }
        // A paint's own presentation damage tells a root readback it changed.
        self.scene_structure_dirty = true;
    }

    /// Mark scene structure as changed. This is the coarse fallback
    /// for map/unmap/configure/restack/redirect/root-background
    /// transitions where old/new visibility cannot yet be expressed
    /// as a narrower rect.
    #[track_caller]
    pub(crate) fn mark_scene_structure_dirty(&mut self) {
        let event_id = if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), self.full_output_audit_area())
        } else {
            None
        };
        self.note_structure_change();
        if let Some(inner) = self.inner.as_mut() {
            let mut contributed = Vec::with_capacity(inner.outputs.len());
            for o in &mut inner.outputs {
                let extent = o.output_extent;
                o.scene_structure_damage.add(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                });
                contributed.push(o.output_idx);
            }
            if let Some(id) = event_id {
                note_damage_audit_contributions(inner, id, &contributed);
            }
        }
    }

    /// Region-precise scene-structure damage (Stage 3+).
    #[track_caller]
    pub(crate) fn mark_scene_structure_damage_rect(&mut self, output_idx: usize, r: vk::Rect2D) {
        let event_id = if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), vec![r])
        } else {
            None
        };
        self.note_structure_change();
        if let Some(inner) = self.inner.as_mut()
            && let Some(o) = inner.outputs.get_mut(output_idx)
        {
            o.scene_structure_damage.add(r);
            if let Some(id) = event_id {
                note_damage_audit_contributions(inner, id, &[output_idx]);
            }
        }
    }

    /// Stage 4c.1 — rect-precise scene-structure damage where the
    /// caller doesn't know which output(s) a screen-/output-coord
    /// rect intersects. Each input rect is intersected against every
    /// output's extent and (if non-empty) added to that output's
    /// `scene_structure_damage`. Mirrors the singular
    /// `mark_scene_structure_damage_rect` setter but applies to all
    /// outputs with output-extent clipping, the dual of
    /// `add_projected_damage` (output-coord input rather than
    /// storage-local projection).
    ///
    /// In the Stage-4 single-output deployment, output origin is
    /// (0, 0) so "screen-coord" and "output-local-coord" coincide;
    /// this clip is just "drop the bits that fall off the right /
    /// bottom edge".
    #[track_caller]
    pub(crate) fn mark_scene_structure_damage_rects(&mut self, rects: &[vk::Rect2D]) {
        let event_id = if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), rects.to_vec())
        } else {
            None
        };
        self.note_structure_change();
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let mut contributed = Vec::new();
        for output_idx in 0..inner.outputs.len() {
            let output = &mut inner.outputs[output_idx];
            let before = output.scene_structure_damage.rects().len();
            dispatch_clip_rects_to_outputs(
                std::iter::once((
                    output.output_origin,
                    output.output_extent,
                    &mut output.scene_structure_damage,
                )),
                rects,
            );
            if output.scene_structure_damage.rects().len() != before {
                contributed.push(output_idx);
            }
        }
        if let Some(id) = event_id {
            note_damage_audit_contributions(inner, id, &contributed);
        }
    }

    /// Toggle an overlay XOR op (root-absolute rects) and inject output damage
    /// so a compose actually runs (wake_for_damage alone leaves output_damage
    /// empty and the frame is EmptyDamage-skipped).
    pub(crate) fn root_overlay_toggle(
        &mut self,
        client: yserver_protocol::x11::ClientId,
        value: u32,
        rects: &[ash::vk::Rect2D],
    ) {
        let outcome = self.root_overlay.toggle(client, value, rects);
        if outcome.changed {
            let mut dmg = rects.to_vec();
            dmg.extend(self.root_overlay.all_rects());
            // An erase that does not exactly match what was drawn inserts a
            // second copy instead of removing the first; the two XOR to
            // identity, so every fresh compose looks right while pixels
            // inverted earlier stay stale — the same symptom as damage that
            // never composed. `removed`/`inserted` separate the two.
            if tick_skip_log_enabled() {
                log::info!(
                    "overlay-diag: value={value:#x} batch={} removed={} inserted={} total={} damaged={}",
                    rects.len(),
                    outcome.removed,
                    outcome.inserted,
                    outcome.total,
                    dmg.len(),
                );
            }
            self.mark_scene_structure_damage_rects(&dmg);
            self.wake_for_damage();
        }
    }

    /// Clear the overlay (RandR/topology change) and damage the vacated rects.
    pub(crate) fn root_overlay_clear(&mut self) {
        if self.root_overlay.is_empty() {
            return;
        }
        let vacated = self.root_overlay.all_rects();
        self.root_overlay.clear();
        self.mark_scene_structure_damage_rects(&vacated);
        self.wake_for_damage();
    }

    /// Drop a disconnecting client's overlay contribution.
    pub(crate) fn root_overlay_on_disconnect(&mut self, client: yserver_protocol::x11::ClientId) {
        let vacated = self.root_overlay.all_rects();
        if self.root_overlay.on_client_disconnect(client) {
            self.mark_scene_structure_damage_rects(&vacated);
            self.wake_for_damage();
        }
    }
}

/// Hand the content damage output `from` just composed to every other output
/// that has not composed it, as structure damage. Returns whether any output
/// took some.
///
/// `from`'s retire acks the drawable's damage in the store, which is global:
/// an output that was flip-pending when the paint landed, and walks only after
/// that retire, would find nothing and keep the old pixels on screen (a caja
/// desktop repaint after a RANDR change, lost on the other monitor). An output
/// whose own compose carried the drawable at this epoch or newer already shows
/// it, and one on which the drawable had no pieces cannot show it.
pub(super) fn fan_out_carried_damage(
    inner: &mut SceneCompositorInner,
    from: usize,
    carried: &[CarriedDamage],
) -> bool {
    let mut took = false;
    for (idx, o) in inner.outputs.iter_mut().enumerate() {
        if idx != from {
            took |= fan_out_to_output(
                o.output_origin,
                o.output_extent,
                &o.last_pieces,
                &o.presented_epochs,
                &mut o.scene_structure_damage,
                carried,
            );
        }
    }
    took
}

/// One output's share of [`fan_out_carried_damage`].
pub(super) fn fan_out_to_output(
    origin: (i32, i32),
    extent: vk::Extent2D,
    last_pieces: &std::collections::HashSet<crate::kms::render::store::DrawableId>,
    presented_epochs: &std::collections::HashMap<crate::kms::render::store::DrawableId, u64>,
    damage: &mut RegionSet,
    carried: &[CarriedDamage],
) -> bool {
    let before = damage.rects().len();
    for c in carried {
        if !last_pieces.contains(&c.id)
            || presented_epochs.get(&c.id).is_some_and(|e| *e >= c.epoch)
        {
            continue;
        }
        dispatch_clip_rects_to_outputs(std::iter::once((origin, extent, &mut *damage)), &c.root);
    }
    damage.rects().len() != before
}

/// Stage 4c.1 — for each `(extent, damage)` pair in `outputs`, clip
/// every rect in `rects` to that output's extent and (if non-empty)
/// add the clipped rect to that output's damage `RegionSet`.
///
/// Extracted from [`SceneCompositor::mark_scene_structure_damage_rects`]
/// so the dispatch + clip + accumulate wiring is unit-testable
/// without needing a live `VkContext` + `CompositorPipeline`.
pub(super) fn dispatch_clip_rects_to_outputs<'a, I>(outputs: I, rects: &[vk::Rect2D])
where
    I: IntoIterator<Item = ((i32, i32), vk::Extent2D, &'a mut RegionSet)>,
{
    for (origin, ext, damage) in outputs {
        for r in rects {
            // Callers pass ROOT-ABSOLUTE rects — `window_absolute_rect` for the
            // scene-participation path, and the root overlay's own root-absolute
            // rects. `clip_rect_to_output_extent` works in output-local space,
            // so the layout origin has to come off first. Omitting it was a
            // latent bug: on a single output at (0,0) the two spaces coincide,
            // but on a multi-output layout with a non-zero origin the damage
            // landed on the wrong output or was clipped away entirely. Note that
            // the overlay's *rendering* path already translates
            // (`apply_list_for_output`), so this was the two halves disagreeing.
            let local = vk::Rect2D {
                offset: vk::Offset2D {
                    x: r.offset.x - origin.0,
                    y: r.offset.y - origin.1,
                },
                extent: r.extent,
            };
            if let Some(clipped) = clip_rect_to_output_extent(local, ext) {
                damage.add(clipped);
            }
        }
    }
}

/// Stage 4c.1 — intersect a rect (in output-local coords) with the
/// output's extent. Returns `None` if the intersection is empty
/// (rect lies fully outside, or input has zero width/height).
///
/// This is the output-local counterpart to `add_projected_damage`'s
/// clipping math: same rectangle-intersection arithmetic, but the
/// projection (the `+dx`/`+dy` translation that maps storage-local
/// coords into output coords) is omitted because the caller already
/// works in output coords.
pub(super) fn clip_rect_to_output_extent(
    rect: vk::Rect2D,
    ext: vk::Extent2D,
) -> Option<vk::Rect2D> {
    let max_x = i32::try_from(ext.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(ext.height).unwrap_or(i32::MAX);
    let x0 = rect.offset.x.max(0);
    let y0 = rect.offset.y.max(0);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .min(max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .min(max_y);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}

pub(super) fn add_projected_damage(
    projected: &mut RegionSet,
    src: vk::Rect2D,
    dx: i32,
    dy: i32,
    layout_w: u32,
    layout_h: u32,
) {
    if let Some(r) = project_onto_output(src, dx, dy, layout_w, layout_h) {
        projected.add(r);
    }
}

/// A storage-local rect translated by the node's output-local origin and
/// clipped to the output; `None` if nothing of it lands on the output.
pub(super) fn project_onto_output(
    src: vk::Rect2D,
    dx: i32,
    dy: i32,
    layout_w: u32,
    layout_h: u32,
) -> Option<vk::Rect2D> {
    let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
    let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);
    let x0 = (src.offset.x + dx).max(0);
    let y0 = (src.offset.y + dy).max(0);
    let x1 = (src.offset.x + dx)
        .saturating_add_unsigned(src.extent.width)
        .min(layout_w_i);
    let y1 = (src.offset.y + dy)
        .saturating_add_unsigned(src.extent.height)
        .min(layout_h_i);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}
