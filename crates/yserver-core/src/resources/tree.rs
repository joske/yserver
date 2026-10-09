use super::*;

impl ResourceTable {
    pub fn window(&self, id: ResourceId) -> Option<&Window> {
        self.windows.get(&id.0)
    }

    pub fn window_mut(&mut self, id: ResourceId) -> Option<&mut Window> {
        self.windows.get_mut(&id.0)
    }

    pub fn windows_iter(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
    }

    pub fn children(&self, parent: ResourceId) -> &[ResourceId] {
        self.windows
            .get(&parent.0)
            .map_or(&[], |window| window.children.as_slice())
    }

    /// Phase-2 naive CirculateWindow: rotate the back child to the front
    /// (`direction = 0`, RaiseLowest) or the front child to the back
    /// (`direction = 1`, LowerHighest). Returns the moved child if any.
    /// Real obscuring detection is a Phase 4+ compositor concern.
    ///
    /// On `ROOT_WINDOW`, the Composite Overlay Window (COW) is excluded from
    /// the rotation — it stays pinned at the top, mirroring Xorg's
    /// `CompositeRealChildHead` semantics.
    /// Restack `child` to the top of its siblings (below the overlay window
    /// on the root) or to the bottom — the move CirculateWindow makes once
    /// `ServerState::circulate_candidate` picked the child.
    pub fn circulate_child(&mut self, child: ResourceId, to_top: bool) {
        self.restack_window(child, None, Some(if to_top { 0 } else { 1 }));
    }

    pub fn mapped_children_bottom_to_top(&self, parent: ResourceId) -> Option<Vec<ResourceId>> {
        let parent = self.windows.get(&parent.0)?;
        Some(
            parent
                .children
                .iter()
                .copied()
                .filter(|child| {
                    self.windows
                        .get(&child.0)
                        .is_some_and(|w| w.map_state != MapState::Unmapped)
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn is_descendant_of(&self, candidate: ResourceId, ancestor: ResourceId) -> bool {
        let mut current = candidate;
        let mut seen = 0usize;
        while current != ROOT_WINDOW && seen <= self.windows.len() {
            let Some(window) = self.windows.get(&current.0) else {
                return false;
            };
            if window.parent == ancestor {
                return true;
            }
            if window.parent == current {
                return false;
            }
            current = window.parent;
            seen += 1;
        }
        false
    }

    pub fn window_owner(&self, id: ResourceId) -> Option<ClientId> {
        self.windows.get(&id.0).map(|w| w.owner)
    }

    pub fn parent_of(&self, id: ResourceId) -> Option<ResourceId> {
        self.windows.get(&id.0).map(|w| w.parent)
    }

    /// Walk mapped descendants of `top_level` and return the intersection
    /// rectangles (in each descendant's local coordinates) that overlap an
    /// expose region given in top-level coordinates. Used to synthesize
    /// `Expose` events for sub-windows when only the top-level subwindow has a
    /// host counterpart — without this, dragging a window across another
    /// leaves child sub-windows (titlebars, content panes) unrepainted because
    /// the host server never knows they exist.
    #[must_use]
    pub fn descendants_in_exposed_area(
        &self,
        top_level: ResourceId,
        ex: i16,
        ey: i16,
        ew: u16,
        eh: u16,
    ) -> Vec<ExposedRect> {
        let mut out = Vec::new();
        let er = i32::from(ex) + i32::from(ew);
        let eb = i32::from(ey) + i32::from(eh);
        self.descendants_in_exposed_area_inner(
            top_level,
            0,
            0,
            i32::from(ex),
            i32::from(ey),
            er,
            eb,
            &mut out,
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn descendants_in_exposed_area_inner(
        &self,
        parent_id: ResourceId,
        parent_x: i32,
        parent_y: i32,
        ex: i32,
        ey: i32,
        er: i32,
        eb: i32,
        out: &mut Vec<ExposedRect>,
    ) {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return;
        };
        for child_id in &parent.children {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let cx = parent_x + i32::from(child.x);
            let cy = parent_y + i32::from(child.y);
            let cr = cx + i32::from(child.width);
            let cb = cy + i32::from(child.height);
            let ix = ex.max(cx);
            let iy = ey.max(cy);
            let ir = er.min(cr);
            let ib = eb.min(cb);
            if ir <= ix || ib <= iy {
                continue;
            }
            let local_x = i16::try_from(ix - cx).unwrap_or(0);
            let local_y = i16::try_from(iy - cy).unwrap_or(0);
            let local_w = u16::try_from(ir - ix).unwrap_or(0);
            let local_h = u16::try_from(ib - iy).unwrap_or(0);
            if local_w > 0 && local_h > 0 {
                out.push(ExposedRect {
                    window: *child_id,
                    x: local_x,
                    y: local_y,
                    width: local_w,
                    height: local_h,
                });
            }
            self.descendants_in_exposed_area_inner(*child_id, cx, cy, ex, ey, er, eb, out);
        }
    }

    pub fn pointer_target_at(
        &self,
        top_level: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        let top = self.windows.get(&top_level.0)?;
        if top.map_state == MapState::Unmapped {
            return None;
        }
        let mut best = (top_level, x, y);
        self.pointer_target_at_inner(top_level, x, y, &mut best);
        Some(best)
    }

    fn pointer_target_at_inner(
        &self,
        parent: ResourceId,
        parent_x: i16,
        parent_y: i16,
        best: &mut (ResourceId, i16, i16),
    ) {
        let Some(parent_window) = self.windows.get(&parent.0) else {
            return;
        };
        for child_id in parent_window.children.iter().rev() {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            // #133 step 8 (P9): hit-test in OUTER space and report
            // CONTENT coordinates. `Window::to_content_coords` /
            // `outer_contains_content_point` hold the rule and its
            // Xorg citation; this must stay identical to
            // `ServerState::hit_test_child`, the other implementation,
            // which additionally gates on the input shape (shape state
            // lives on `ServerState`, not here).
            let (child_x, child_y) = child.to_content_coords(parent_x, parent_y);
            if !child.outer_contains_content_point(child_x, child_y) {
                continue;
            }
            *best = (*child_id, child_x, child_y);
            self.pointer_target_at_inner(*child_id, child_x, child_y, best);
            return;
        }
    }

    /// The errors [`Self::reparent_window`] would return, without reparenting.
    ///
    /// # Errors
    ///
    /// `BadMatch` for the root, the window itself or one of its inferiors as
    /// the new parent; `BadWindow` for an unknown window or parent.
    pub fn check_reparent_window(
        &self,
        request: ReparentWindowRequest,
    ) -> Result<(), ReparentWindowError> {
        if request.window == ROOT_WINDOW
            || request.window == request.parent
            || self.is_descendant_of(request.parent, request.window)
        {
            return Err(ReparentWindowError::BadMatch);
        }
        if !self.windows.contains_key(&request.window.0)
            || !self.windows.contains_key(&request.parent.0)
        {
            return Err(ReparentWindowError::BadWindow);
        }
        Ok(())
    }

    pub fn reparent_window(
        &mut self,
        request: ReparentWindowRequest,
    ) -> Result<ReparentResult, ReparentWindowError> {
        self.check_reparent_window(request)?;
        let window = self
            .windows
            .get(&request.window.0)
            .expect("window validated above");

        let old_parent = window.parent;
        let override_redirect = window.override_redirect;
        let host_xid = window.host_xid;
        let old_map_state = window.map_state;

        if let Some(parent) = self.windows.get_mut(&old_parent.0) {
            parent.children.retain(|child| *child != request.window);
        }
        if let Some(parent) = self.windows.get_mut(&request.parent.0) {
            let insert_at = cow_aware_top_index(parent);
            parent.children.insert(insert_at, request.window);
        }
        let parent_viewable = self
            .windows
            .get(&request.parent.0)
            .is_some_and(|p| p.map_state == MapState::Viewable);
        if log::log_enabled!(target: "yserver::input::restack", log::Level::Trace) {
            log::trace!(
                target: "yserver::input::restack",
                "REPARENT win=0x{:x} old_parent=0x{:x} new_parent=0x{:x} -> inserted at top | root after: [{}]",
                request.window.0,
                old_parent.0,
                request.parent.0,
                self.debug_root_order(),
            );
        }
        let window = self
            .windows
            .get_mut(&request.window.0)
            .expect("window validated above");
        window.parent = request.parent;
        window.x = request.x;
        window.y = request.y;
        // A mapped window's effective viewability follows the new parent. This
        // must cascade to the whole subtree: reparenting a mapped subtree under
        // a non-viewable parent leaves it mapped-but-Unviewable (and vice-versa).
        // Without the cascade a Viewable child can survive under an Unviewable
        // ancestor, and damage/scene/Expose fanout (which filter on
        // map_state == Viewable) then mis-handle it.
        let propagate = window.map_state != MapState::Unmapped;
        if propagate {
            window.map_state = if parent_viewable {
                MapState::Viewable
            } else {
                MapState::Unviewable
            };
        }
        let new_map_state = window.map_state;
        let was_viewable = old_map_state == MapState::Viewable;
        let mut delta = ViewabilityDelta::default();
        if propagate {
            if parent_viewable {
                if !was_viewable {
                    delta.became_viewable.push(request.window);
                }
                self.promote_unviewable_descendants(request.window, &mut delta.became_viewable);
            } else {
                self.demote_viewable_descendants(request.window, &mut delta.became_unviewable);
                if was_viewable {
                    delta.became_unviewable.push(request.window);
                }
            }
        }
        // Phase 3.6 Step 4a forwards XReparentWindow to the host, so
        // the host subwindow stays alive and continues to be the
        // rendering target. (Pre-Step-4a code destroyed the host
        // subwindow when a top-level moved away from root and cleared
        // host_xid here; that's no longer correct.)

        Ok(ReparentResult {
            window: request.window,
            old_parent,
            new_parent: request.parent,
            x: request.x,
            y: request.y,
            override_redirect,
            host_xid,
            old_map_state,
            new_map_state,
            delta,
        })
    }

    /// Returns the screen-absolute (x, y) of the top-left corner of `id`.
    /// Walks up the parent chain accumulating x/y offsets. Returns (0, 0) for ROOT_WINDOW.
    #[must_use]
    pub fn window_absolute_position(&self, id: ResourceId) -> (i32, i32) {
        if id == ROOT_WINDOW {
            return (0, 0);
        }
        let mut ax: i32 = 0;
        let mut ay: i32 = 0;
        let mut current = id;
        let mut depth = 0usize;
        while current != ROOT_WINDOW && depth < 256 {
            let Some(w) = self.windows.get(&current.0) else {
                break;
            };
            // #133 step 7 (P7) — a window's x/y locate its OUTER
            // upper-left corner relative to the parent's origin, so its
            // own origin, where its contents and children start, sits
            // `border_width` further in. Xorg keeps exactly this sum
            // pre-computed in `drawable.x`:
            //   pWin->drawable.x = pParent->drawable.x + x + (int) bw;
            // (`dix/window.c:888`). Summing only x/y made every
            // root-relative coordinate short by the border width of each
            // window in the chain.
            //
            // This re-lands `7e1484b4`, which was reverted by `d08d6933`
            // with an empty message. Its own note said the term is
            // "invisible on real desktops, where window managers use
            // border_width 0" — awesome is the counter-example, and with
            // a 16px border the shortfall is what put pointer hit-spots
            // a border width away from the widget the client drew.
            // Oracle: xts5 XI/GrabDeviceButton-4, which measured x_root
            // 103 against an expected 104.
            ax += i32::from(w.x) + i32::from(w.border_width);
            ay += i32::from(w.y) + i32::from(w.border_width);
            if w.parent == current {
                break;
            }
            current = w.parent;
            depth += 1;
        }
        (ax, ay)
    }

    /// Returns the child of `parent` (in the nested window tree) that contains
    /// the screen-absolute point (abs_x, abs_y), or None if no mapped child contains it.
    /// Children are checked in reverse order (top of stacking = last in list).
    #[must_use]
    pub fn child_containing_point(
        &self,
        parent: ResourceId,
        abs_x: i32,
        abs_y: i32,
    ) -> Option<ResourceId> {
        // `window_absolute_position` returns the parent's CONTENT
        // origin (#133 step 7, `dix/window.c:888`), and a child's
        // `x`/`y` are relative to exactly that, so `local_*` is already
        // in the parent's content space — the space a child's outer box
        // is expressed in.
        let (px, py) = self.window_absolute_position(parent);
        let local_x = abs_x - px;
        let local_y = abs_y - py;
        let w = self.windows.get(&parent.0)?;
        for &child_id in w.children.iter().rev() {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            // #133 step 8 (P9): test the BORDER-INCLUSIVE box, not the
            // content box. Xorg's `PointInWindowIsVisible` tests
            // `pWin->borderClip` (`dix/window.c:2993`), which
            // `SetBorderSize` builds from the outer rectangle.
            // `window_bounding_box` is that outer rectangle
            // (`x .. x + width + 2 * bw`) and is already what the
            // stacking-overlap tests use; at `bw == 0` it is the old
            // content box exactly. Kept in step with
            // `Window::outer_contains_content_point`, which states the
            // same region relative to the content origin — the
            // `hit_test_implementations_agree_*` tests pin the two
            // together.
            let (cx, cy, cr, cb) = window_bounding_box(child);
            if local_x >= cx && local_x < cr && local_y >= cy && local_y < cb {
                return Some(child_id);
            }
        }
        None
    }
}
