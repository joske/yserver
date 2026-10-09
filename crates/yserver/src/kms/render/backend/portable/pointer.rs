use super::*;

impl KmsBackend {
    /// Deepest mapped window under the cursor. Walks
    /// `core.top_level_order` back-to-front for the topmost top-level
    /// match, then descends the sub-window tree picking the topmost
    /// mapped child at each level whose screen-coords box contains
    /// the cursor. SHAPE-input (or bounding) trims the hittable
    /// region at every level.
    ///
    /// Why descend: xfwm4 attaches resize-edge cursors to thin frame
    /// sub-windows (one child per edge under each frame top-level),
    /// not to the frame top-level itself. Without sub-window descent
    /// the pointer-window stays pinned to the frame, the cursor walk
    /// in `effective_cursor_walking_chain` picks up only the frame's
    /// (`None`) cursor + the root fallback, and the resize sprites
    /// never become effective — the cursor stays as the default
    /// arrow over xfwm4 frame edges. Matches Xorg `dix/events.c`'s
    /// `XYToWindow` descent. The depth bound mirrors the cursor
    /// walk's 64.
    /// Resolve the window under the cursor through the **same authority
    /// the event fanout uses for delivery** — the core resource-tree
    /// hit-test (`root_pointer_target_at`) over `shape_windows` — and map
    /// it to a host XID for the existing crossing/motion emit machinery.
    ///
    /// This replaces the backend's parallel host-space hit-test
    /// (`window_under_cursor` over `top_level_order`/`windows` +
    /// `core.shape_input`) as the crossing/motion *producer*. The two
    /// disagreed on Cinnamon's Composite Overlay Window: the backend store
    /// cannot represent an empty input region (`set_shape_rectangles`
    /// removes the entry, which `cursor_inside_shape` reads as opaque),
    /// while the core store keeps `Some([])` = click-through. So the
    /// producer emitted Enter/Leave into the COW/stage subtree while
    /// delivery (`root_pointer_target_at`) resolved the app beneath →
    /// sloppy focus silently failed. Resolving the producer through the
    /// same tree+coords as delivery makes the two agree by construction
    /// (Xorg's single-sprite-trace model).
    ///
    /// `cursor_x`/`cursor_y` are cast to `i16` exactly as the emit path
    /// stamps `root_x`/`root_y`, so the producer and the fanout's
    /// `root_pointer_target_at(event.root_x, event.root_y)` see identical
    /// inputs. Falls back to the root container when the cursor resolves to
    /// the root window or the hit window has no host backing.
    #[allow(clippy::cast_possible_truncation)]
    pub(in crate::kms::render::backend) fn resource_pointer_host_xid(
        &mut self,
        server_state: &ServerState,
    ) -> u32 {
        self.prune_input_only_pointer_hosts(server_state);
        let Some((mut resid, _, _)) = server_state
            .root_pointer_target_at(self.core.cursor_x as i16, self.core.cursor_y as i16)
        else {
            return self.core.window_id;
        };
        if let Some(host) = self.input_only_pointer_host(server_state, resid) {
            return host;
        }
        // Walk up to the nearest host-backed window so a non-hosted
        // sub-window resolves to the deepest *hosted* ancestor — matching
        // the granularity of the host walk this replaces, rather than
        // collapsing straight to the root container.
        for _ in 0..256 {
            if resid == yserver_core::resources::ROOT_WINDOW {
                break;
            }
            let Some(w) = server_state.resources.window(resid) else {
                break;
            };
            if let Some(h) = w.host_xid {
                return h.as_raw();
            }
            resid = w.parent;
        }
        self.core.window_id
    }

    /// The synthetic host standing in for `window` when it is an InputOnly
    /// window with no backend window, allocated on first use. Refreshes the
    /// entry's parent host and cursor from the core tree.
    fn input_only_pointer_host(
        &mut self,
        server_state: &ServerState,
        window: ResourceId,
    ) -> Option<u32> {
        let w = server_state.resources.window(window)?;
        if w.class != yserver_core::resources::WindowClass::InputOnly || w.host_xid.is_some() {
            return None;
        }
        let mut cursor = None;
        let mut parent_host = self.core.window_id;
        let mut cur = window;
        for _ in 0..256 {
            if cur == yserver_core::resources::ROOT_WINDOW {
                break;
            }
            let Some(cw) = server_state.resources.window(cur) else {
                break;
            };
            if let Some(h) = cw.host_xid {
                parent_host = h.as_raw();
                break;
            }
            // The window's own hold on its cursor, which outlives the
            // cursor's XID (Xorg refcnt).
            if cursor.is_none() {
                cursor = cw.cursor_host.map(CursorHandle::as_raw);
            }
            cur = cw.parent;
        }
        let host = self
            .input_only_pointer_hosts
            .iter()
            .find_map(|(h, e)| (e.window == window).then_some(*h))
            .unwrap_or_else(|| {
                let h = self.core.next_host_xid();
                self.core.xid_map.insert(h, window);
                h
            });
        self.input_only_pointer_hosts.insert(
            host,
            InputOnlyPointerHost {
                window,
                parent_host,
                cursor,
                origin: (w.x, w.y),
            },
        );
        Some(host)
    }

    /// Drop the stand-in hosts of InputOnly windows that are gone (or no
    /// longer InputOnly without a backend window), as a destroyed window's
    /// host leaves `xid_map`.
    fn prune_input_only_pointer_hosts(&mut self, server_state: &ServerState) {
        let stale: Vec<u32> = self
            .input_only_pointer_hosts
            .iter()
            .filter(|(_, e)| {
                !server_state.resources.window(e.window).is_some_and(|w| {
                    w.class == yserver_core::resources::WindowClass::InputOnly
                        && w.host_xid.is_none()
                })
            })
            .map(|(h, _)| *h)
            .collect();
        for h in stale {
            self.input_only_pointer_hosts.remove(&h);
            self.core.xid_map.remove(&h);
        }
    }

    /// The host a crossing event on `window` is stamped with: its backend
    /// window, the stand-in of an InputOnly window, else `fallback`.
    fn crossing_host_for(
        &mut self,
        server_state: &ServerState,
        window: ResourceId,
        fallback: u32,
    ) -> u32 {
        if window == yserver_core::resources::ROOT_WINDOW {
            return self.core.window_id;
        }
        if let Some(h) = server_state
            .resources
            .window(window)
            .and_then(|w| w.host_xid)
        {
            return h.as_raw();
        }
        self.input_only_pointer_host(server_state, window)
            .unwrap_or(fallback)
    }

    #[allow(dead_code)] // retained for A/B comparison vs the resource-tree producer + unit tests
    pub(in crate::kms::render::backend) fn window_under_cursor(&self) -> Option<u32> {
        let cx = f64::from(self.core.cursor_x);
        let cy = f64::from(self.core.cursor_y);
        let mut hit: Option<(u32, f64, f64)> = None;
        for &window_id in self.core.top_level_order.iter().rev() {
            let Some(w) = self.windows.get(&window_id) else {
                log::trace!(
                    target: "yserver::kms::render::pointer",
                    "wuc: skip 0x{window_id:x} (not in windows)"
                );
                continue;
            };
            if !w.mapped {
                log::trace!(
                    target: "yserver::kms::render::pointer",
                    "wuc: skip 0x{window_id:x} (unmapped)"
                );
                continue;
            }
            let wx = f64::from(w.x);
            let wy = f64::from(w.y);
            if cx < wx || cx >= wx + f64::from(w.width) || cy < wy || cy >= wy + f64::from(w.height)
            {
                log::trace!(
                    target: "yserver::kms::render::pointer",
                    "wuc: skip 0x{window_id:x} cursor=({cx},{cy}) outside geom=({},{} {}x{})",
                    w.x, w.y, w.width, w.height
                );
                continue;
            }
            if !self.cursor_inside_shape(window_id, cx - wx, cy - wy) {
                log::trace!(
                    target: "yserver::kms::render::pointer",
                    "wuc: skip 0x{window_id:x} local=({},{}) SHAPE-excluded (geom={},{} {}x{})",
                    cx - wx, cy - wy, w.x, w.y, w.width, w.height
                );
                continue;
            }
            log::trace!(
                target: "yserver::kms::render::pointer",
                "wuc: HIT 0x{window_id:x} cursor=({cx},{cy}) local=({},{}) geom=({},{} {}x{})",
                cx - wx, cy - wy, w.x, w.y, w.width, w.height
            );
            hit = Some((window_id, wx, wy));
            break;
        }
        let (mut parent_xid, mut parent_x, mut parent_y) = hit?;
        for _ in 0..64 {
            let mut children: Vec<(u32, u64, i16, i16, u16, u16)> = self
                .windows
                .iter()
                .filter_map(|(xid, g)| {
                    (g.parent == Some(parent_xid) && g.mapped).then_some((
                        *xid,
                        g.stack_rank,
                        g.x,
                        g.y,
                        g.width,
                        g.height,
                    ))
                })
                .collect();
            children.sort_by_key(|c| std::cmp::Reverse(c.1));
            let mut next: Option<(u32, f64, f64)> = None;
            for (child_id, _rank, cxoff, cyoff, cw, ch) in children {
                let cax = parent_x + f64::from(cxoff);
                let cay = parent_y + f64::from(cyoff);
                if cx < cax || cx >= cax + f64::from(cw) || cy < cay || cy >= cay + f64::from(ch) {
                    continue;
                }
                if !self.cursor_inside_shape(child_id, cx - cax, cy - cay) {
                    continue;
                }
                next = Some((child_id, cax, cay));
                break;
            }
            match next {
                Some((child, cax, cay)) => {
                    parent_xid = child;
                    parent_x = cax;
                    parent_y = cay;
                }
                None => break,
            }
        }
        Some(parent_xid)
    }

    /// SHAPE-input (preferred) / bounding (fallback) hit-test for a
    /// single window. `local_x`/`local_y` are the pointer position
    /// in the window's own coordinate space (origin = window's top-
    /// left). Returns `true` when no SHAPE is set or the cursor lies
    /// inside at least one rectangle; an empty rect list means the
    /// window is unhittable.
    fn cursor_inside_shape(&self, window_id: u32, local_x: f64, local_y: f64) -> bool {
        let shape = self
            .core
            .shape_input
            .get(&window_id)
            .or_else(|| self.core.shape_bounding.get(&window_id));
        let Some(rects) = shape else {
            return true;
        };
        rects.iter().any(|r| {
            let rx = f64::from(r.x);
            let ry = f64::from(r.y);
            local_x >= rx
                && local_x < rx + f64::from(r.width)
                && local_y >= ry
                && local_y < ry + f64::from(r.height)
        })
    }

    /// Event-window-relative coords for an event whose `host_xid`
    /// is the topmost mapped top-level under the cursor. v2-shape
    /// port — reads geometry off `windows`. Falls back to root
    /// coords when `host_xid` isn't tracked (the dispatcher
    /// re-derives target coords from its own tree walk anyway).
    fn event_relative_coords(&self, host_xid: u32) -> (i16, i16) {
        let origin = self.windows.get(&host_xid).map(|w| (w.x, w.y)).or_else(|| {
            self.input_only_pointer_hosts
                .get(&host_xid)
                .map(|e| e.origin)
        });
        if let Some((x, y)) = origin {
            let ex = (self.core.cursor_x as i32) - i32::from(x);
            let ey = (self.core.cursor_y as i32) - i32::from(y);
            (
                ex.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                ey.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            )
        } else {
            (self.core.cursor_x as i16, self.core.cursor_y as i16)
        }
    }

    fn emit_pointer(&mut self, ev: HostPointerEvent) {
        self.core.pending_pointer_events.push(ev);
    }

    fn emit_crossing(
        &mut self,
        origin: yserver_core::core_loop::InputOrigin,
        host_xid: u32,
        kind: PointerEventKind,
        detail: u8,
        crossing_mode: u8,
        child: u32,
        state: u16,
    ) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            origin,
            kind,
            host_xid,
            detail,
            time: crate::clock::server_time_ms(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode,
            child,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        };
        self.emit_pointer(ev);
    }

    fn emit_motion_only(
        &mut self,
        origin: yserver_core::core_loop::InputOrigin,
        host_xid: u32,
        mask: u16,
        raw_dx: i32,
        raw_dy: i32,
    ) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            origin,
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time: crate::clock::server_time_ms(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state: mask,
            crossing_mode: 0,
            child: 0,
            raw_dx,
            raw_dy,
            tree_change: false,
        };
        self.emit_pointer(ev);
    }

    pub(in crate::kms::render::backend) fn emit_floating_pointer_event_at(
        &mut self,
        server_state: &ServerState,
        origin: yserver_core::core_loop::InputOrigin,
        kind: PointerEventKind,
        detail: u8,
        root_x: i16,
        root_y: i16,
        state_mask: u16,
        raw_dx: i32,
        raw_dy: i32,
    ) {
        let previous = (self.core.cursor_x, self.core.cursor_y);
        self.core.cursor_x = f32::from(root_x);
        self.core.cursor_y = f32::from(root_y);
        let host_xid = self.resource_pointer_host_xid(server_state);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        self.emit_pointer(HostPointerEvent {
            origin,
            kind,
            host_xid,
            detail,
            time: crate::clock::server_time_ms(),
            root_x,
            root_y,
            event_x,
            event_y,
            state: state_mask,
            crossing_mode: 0,
            child: 0,
            raw_dx,
            raw_dy,
            tree_change: false,
        });
        self.core.cursor_x = previous.0;
        self.core.cursor_y = previous.1;
    }

    /// Spec-correct Normal-mode crossing chain for a top-level
    /// transition. Direct v1 port (kms/backend.rs:6630-6695) —
    /// the body only touches KmsCore + nested-resource look-ups.
    pub(in crate::kms::render::backend) fn update_pointer_window(
        &mut self,
        server_state: &ServerState,
        new_xid: u32,
        mask: u16,
        origin: yserver_core::core_loop::InputOrigin,
    ) {
        if self.core.prev_pointer_window == Some(new_xid) {
            log::trace!(
                target: "yserver::kms::render::pointer",
                "upw: SKIP-SAME prev=new=0x{new_xid:x}"
            );
            return;
        }
        let prev_host = self.core.prev_pointer_window;
        let root_container_host = self.core.window_id;
        let resolve_host_to_nested = |host: u32, xid_map: &HostXidMap| -> Option<ResourceId> {
            if host == root_container_host {
                Some(yserver_core::resources::ROOT_WINDOW)
            } else {
                xid_map.get(&host).copied()
            }
        };
        let prev_id = prev_host.and_then(|p| resolve_host_to_nested(p, &self.core.xid_map));
        let new_id = resolve_host_to_nested(new_xid, &self.core.xid_map);
        log::trace!(
            target: "yserver::kms::render::pointer",
            "upw: prev_host={:?} new_host=0x{:x} prev_nested={:?} new_nested={:?}",
            prev_host.map(|h| format!("0x{h:x}")),
            new_xid,
            prev_id.map(|r| r.0),
            new_id.map(|r| r.0),
        );

        if let (Some(from), Some(to)) = (prev_id, new_id) {
            let events = yserver_core::crossings::normal_mode_crossings(server_state, from, to);
            log::trace!(
                target: "yserver::kms::render::pointer",
                "upw: normal_mode_crossings(from={}, to={}) → {} events",
                from.0, to.0, events.len()
            );
            for ev in events {
                let win_host_xid = self.crossing_host_for(server_state, ev.window, new_xid);
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                log::trace!(
                    target: "yserver::kms::render::pointer",
                    "upw: emit_crossing host=0x{win_host_xid:x} kind={:?} detail={} child={:#x}",
                    kind, ev.detail, ev.child.0
                );
                self.emit_crossing(origin, win_host_xid, kind, ev.detail, 0, ev.child.0, mask);
            }
        } else {
            log::trace!(
                target: "yserver::kms::render::pointer",
                "upw: FALLBACK path (prev_id={:?}, new_id={:?})",
                prev_id, new_id
            );
            // First-motion bootstrap or unmapped host_xid —
            // fall back to a single Leave/Enter with detail=0.
            if let Some(prev) = prev_host {
                self.emit_crossing(origin, prev, PointerEventKind::LeaveNotify, 0, 0, 0, mask);
            }
            self.emit_crossing(
                origin,
                new_xid,
                PointerEventKind::EnterNotify,
                0,
                0,
                0,
                mask,
            );
        }
        self.core.prev_pointer_window = Some(new_xid);
        // Stage 5 Phase A: cross-in may change the effective cursor
        // (per-window DefineCursor walks up the parent chain).
        self.refresh_effective_cursor();
    }

    fn dispatch_motion_event(
        &mut self,
        server_state: &ServerState,
        raw_dx: i32,
        raw_dy: i32,
        origin: yserver_core::core_loop::InputOrigin,
    ) {
        // Fall back to the root container so root-window subscribers
        // (e16's right-click-desktop menu, fvwm3's root bindings) can
        // see motion when the cursor is over the wallpaper.
        let host_xid = self.resource_pointer_host_xid(server_state);
        let mask = self.serialize_modifiers() | self.core.button_mask;
        log::trace!(
            target: "yserver::kms::render::pointer",
            "dispatch_motion: cursor=({},{}) → host_xid=0x{host_xid:x}",
            self.core.cursor_x, self.core.cursor_y
        );
        self.update_pointer_window(server_state, host_xid, mask, origin);
        self.emit_motion_only(origin, host_xid, mask, raw_dx, raw_dy);
    }

    /// Every live CRTC's root rectangle (its footprint at its origin).
    pub(in crate::kms::render::backend) fn crtc_root_rects(
        &self,
    ) -> Vec<crate::kms::render::pointer_confine::CrtcRect> {
        (0..self.platform.outputs.len())
            .map(|idx| self.platform.output_root_rect(idx))
            .collect()
    }

    /// `RRPointerScreenConfigured`: after a layout change, a pointer outside
    /// every CRTC moves to the nearest one, with the events of a warp.
    pub(in crate::kms::render::backend) fn move_pointer_to_nearest_crtc(
        &mut self,
        state: &mut ServerState,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let (x, y) = (self.core.cursor_x as i32, self.core.cursor_y as i32);
        let Some((nx, ny)) = crate::kms::render::pointer_confine::nearest_crtc_position(
            &self.crtc_root_rects(),
            x,
            y,
        ) else {
            return;
        };
        self.warp_pointer_root(state, nx, ny);
    }

    pub(in crate::kms::render::backend) fn process_pointer_absolute(
        &mut self,
        server_state: &mut ServerState,
        mut x: f32,
        mut y: f32,
        relative: bool,
        raw_dx: i32,
        raw_dy: i32,
        origin: yserver_core::core_loop::InputOrigin,
    ) {
        // Apply active-grab confinement before hit-testing and crossing
        // generation.  The core fanout also clamps as a backend-independent
        // safety net, but that is too late for KMS: dispatch_motion_event()
        // first calls update_pointer_window(), which would otherwise emit a
        // Leave(game) / Enter(window-below) pair for the unconfined physical
        // coordinate.  Focus-follows-mouse WMs such as dwm react to that
        // EnterNotify by focusing the window below; SDL then drops and
        // reacquires its grab continuously (#99).
        let confine_to = server_state.pointer_confine_to;
        if confine_to.0 != 0
            && let Some(window) = server_state.resources.window(confine_to)
            && window.map_state == yserver_core::resources::MapState::Viewable
        {
            let (x0, y0) = server_state.resources.window_absolute_position(confine_to);
            let x1 = x0 + i32::from(window.width);
            let y1 = y0 + i32::from(window.height);
            let confined_x = x.clamp(x0 as f32, (x1 - 1).max(x0) as f32);
            let confined_y = y.clamp(y0 as f32, (y1 - 1).max(y0) as f32);
            x = confined_x;
            y = confined_y;
        }

        // Clamp to the UNION framebuffer extent (`fb_w`/`fb_h`),
        // not the first output's box. `core_platform_init`
        // (`kms/backend.rs:1063-1072`) computes this as
        // `max(x + width)` across every output, which is also the
        // extent the input thread uses when mapping absolute events. Pre-fix
        // this consulted `outputs.first().width/height`, so the
        // pointer could never cross from output 0 onto a side-
        // adjacent output 1 — pinned by
        // `process_pointer_absolute_uses_union_fb_extent_for_multi_output`.
        let fb_w = f32::from(self.platform.fb_w.max(1));
        let fb_h = f32::from(self.platform.fb_h.max(1));
        let root_x = x.clamp(0.0, (fb_w - 1.0).max(0.0));
        let root_y = y.clamp(0.0, (fb_h - 1.0).max(0.0));
        // Then onto the CRTCs, before any hit-test or event (spec D5b).
        let (new_x, new_y) = crate::kms::render::pointer_confine::constrain_to_crtcs(
            &self.crtc_root_rects(),
            (self.core.cursor_x, self.core.cursor_y),
            (root_x, root_y),
        );
        if new_x != self.core.cursor_x || new_y != self.core.cursor_y {
            self.core.cursor_x = new_x;
            self.core.cursor_y = new_y;
            // Stage 5 Phase D — pointer fast path. When the plane
            // is fully bound on every output AND no transition is
            // pending, route motion directly through
            // `cursor_plane_move` — one ioctl per visible CRTC, no
            // GPU work, no compose cadence. The Mixed state (any
            // output still has a Sw→Hw or Hw→Sw transition
            // pending) falls back to the scene-wake path so the
            // SW cursor doesn't desync from the eventual plane
            // bind. The core thread owns DRM state, so this is
            // not a thread-safety question — the ioctl is
            // synchronous from the same thread that owns scene
            // state.
            let cursor_mode = self.scene.cursor_mode();
            if self.cursor_hidden {
                // XFIXES HideCursor: there is no sprite to move. Showing
                // it again displays it at the then-current position.
            } else if matches!(
                cursor_mode,
                crate::kms::render::scene::CursorPlaneMode::Hw
                    | crate::kms::render::scene::CursorPlaneMode::Mixed
            ) {
                #[allow(clippy::cast_possible_truncation)]
                let cx = new_x as i32;
                #[allow(clippy::cast_possible_truncation)]
                let cy = new_y as i32;
                let (hot_x, hot_y, cw, ch) = self
                    .effective_cursor_xid
                    .and_then(|xid| self.cursor_records.get(&xid))
                    .map(|rec| {
                        (
                            rec.hot_x,
                            rec.hot_y,
                            i32::from(rec.width),
                            i32::from(rec.height),
                        )
                    })
                    .unwrap_or((0, 0, 0, 0));
                if matches!(
                    cursor_mode,
                    crate::kms::render::scene::CursorPlaneMode::Mixed
                ) {
                    // Hardware and software cursor ownership may coexist on
                    // arbitrary cards. Move every visible HW plane immediately
                    // and also repaint the SW outputs.
                    match self.platform.cursor_plane_move(cx, cy, hot_x, hot_y) {
                        Ok(outcome) => {
                            self.handle_cursor_move_outcome(outcome);
                        }
                        Err(e) => log::debug!("render cursor mixed path: move failed: {e}"),
                    }
                    self.scene.wake_for_damage();
                } else if !self.scanout_m2.active()
                    && cw > 0
                    && ch > 0
                    && self
                        .platform
                        .cursor_crtc_membership_dirty(cx, cy, hot_x, hot_y, cw, ch)
                {
                    // The cursor footprint crossed onto/off a CRTC the
                    // plane isn't bound to. The move-only fast path
                    // can't rebind across CRTCs, so route this motion
                    // through one compose tick — the scene's
                    // `CursorAssignment` then issues the show/hide on
                    // retire. Pre-#30 the continuous idle redraw loop
                    // masked this; idle desktops now stop compositing,
                    // so the seam crossing must be detected here or the
                    // cursor stays frozen on the CRTC it was last bound
                    // to (invisible on the screen it moved onto).
                    self.scene.wake_for_damage();
                } else {
                    match self.platform.cursor_plane_move(cx, cy, hot_x, hot_y) {
                        Ok(outcome) => {
                            self.handle_cursor_move_outcome(outcome);
                        }
                        Err(e) => log::debug!("render cursor fast path: move failed: {e}"),
                    }
                }
            } else {
                self.scene.wake_for_damage();
            }
        }
        let prev = server_state.barrier_bypass;
        server_state.barrier_bypass = prev || !relative;
        self.core.pending_motion_barrier_bypass = !relative;
        self.dispatch_motion_event(server_state, raw_dx, raw_dy, origin);
        server_state.barrier_bypass = prev;
    }

    /// Diagnose an unbalanced button transition against the generating
    /// device's held set. Core button state is aggregated across sources, so
    /// it cannot distinguish a duplicate press on one source from a valid
    /// press while another source holds the same button.
    pub(in crate::kms::render::backend) fn button_diagnostic(
        state: &ServerState,
        origin: yserver_core::core_loop::InputOrigin,
        button_bit: u16,
        pressed: bool,
    ) -> Option<KmsButtonDiagnostic> {
        if button_bit == 0 {
            return None;
        }

        use yserver_core::{
            core_loop::InputOrigin,
            xinput::{DEVICEID_MASTER_POINTER, XiFacetKind},
        };

        let device_buttons_down = match origin {
            InputOrigin::Physical(source_id) => state
                .xi_devices
                .facet(source_id, XiFacetKind::PointerTouch)
                .and_then(|device_id| state.xi_devices.device(device_id))
                .map(|device| device.buttons_down)
                .or_else(|| {
                    state
                        .unpublished_pointer_buttons_down
                        .get(&source_id)
                        .copied()
                })
                .unwrap_or(0),
            InputOrigin::XTest(DEVICEID_MASTER_POINTER) | InputOrigin::NestedHost => {
                state.buttons_down
            }
            InputOrigin::XTest(device_id) => state
                .xi_devices
                .device(device_id)
                .map_or(0, |device| device.buttons_down),
        };
        let held_mask = (device_buttons_down & 0x001f) << 8;
        let already_held = held_mask & button_bit != 0;

        match (pressed, already_held) {
            (true, true) => Some(KmsButtonDiagnostic::PressAlreadyHeld { mask: held_mask }),
            (false, false) => Some(KmsButtonDiagnostic::ReleaseNotHeld { mask: held_mask }),
            _ => None,
        }
    }

    pub(in crate::kms::render::backend) fn process_pointer_button(
        &mut self,
        code: u32,
        pressed: bool,
        server_state: &ServerState,
        origin: yserver_core::core_loop::InputOrigin,
    ) {
        let detail = match code {
            0x110 => 1,  // BTN_LEFT
            0x111 => 3,  // BTN_RIGHT
            0x112 => 2,  // BTN_MIDDLE
            0x113 => 8,  // BTN_SIDE
            0x114 => 9,  // BTN_EXTRA
            0x115 => 10, // BTN_FORWARD -> X 10 via btn_linux2xorg (xf86-input-libinput/src/xf86libinput.c:253-272)
            0x180 => 4,  // SYNTH_SCROLL_UP
            0x181 => 5,  // SYNTH_SCROLL_DOWN
            0x182 => 6,  // SYNTH_SCROLL_LEFT
            0x183 => 7,  // SYNTH_SCROLL_RIGHT
            _ => {
                log::debug!("render: unmapped libinput button code 0x{code:x}, dropping");
                return;
            }
        };
        let host_xid = self.resource_pointer_host_xid(server_state);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let button_bit: u16 = match detail {
            1 => 0x0100,
            2 => 0x0200,
            3 => 0x0400,
            4 => 0x0800,
            5 => 0x1000,
            _ => 0,
        };
        let modifier_mask = self.serialize_modifiers();
        // X11 spec: `state` is the logical button state IMMEDIATELY
        // BEFORE the event takes effect. Press: button bit not yet
        // set. Release: button bit still set.
        let master_button_mask = (server_state.buttons_down & 0x001f) << 8;
        let state = if pressed {
            modifier_mask | master_button_mask
        } else {
            modifier_mask | master_button_mask | button_bit
        };
        // Stuck-button / lost-release diagnostic (2026-07-11 drag-select bug).
        // Button state must stay balanced for each generating device: a press
        // for a bit already set — or a release for a bit already clear — means
        // an event was lost or a grab/focus transition desynced us, and text-
        // selection drags break until the mask is cleared. Keep WARN so the
        // intermittent bug captures the event that trips the mismatch.
        // `detail == 4/5` wheel press+release pairs balance normally; 6/7 are
        // not tracked in the button mask.
        match Self::button_diagnostic(server_state, origin, button_bit, pressed) {
            Some(KmsButtonDiagnostic::PressAlreadyHeld { mask }) => {
                log::warn!(
                    "render: ButtonPress detail={detail} but button already held \
                     (mask=0x{mask:04x}) — a prior ButtonRelease was lost; drags/\
                     selection will misbehave until the mask clears",
                );
            }
            Some(KmsButtonDiagnostic::ReleaseNotHeld { mask }) => {
                log::warn!(
                    "render: ButtonRelease detail={detail} but button not marked held \
                     (mask=0x{mask:04x}) — a prior ButtonPress was lost or state desynced",
                );
            }
            None => {}
        }
        if pressed {
            self.core.button_mask |= button_bit;
        } else {
            self.core.button_mask &= !button_bit;
        }
        let time = crate::clock::server_time_ms();
        let kind = if pressed {
            PointerEventKind::ButtonPress
        } else {
            PointerEventKind::ButtonRelease
        };
        let ptr_event = HostPointerEvent {
            origin,
            kind,
            host_xid,
            detail,
            time,
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        };
        self.emit_pointer(ptr_event);
        // Implicit-grab crossings (G3). Direct v1 port.
        let post_state = self.serialize_modifiers() | self.core.button_mask;
        let press_mode: u8 = if pressed { 1 } else { 2 };
        let grab_id = self.core.xid_map.get(&host_xid).copied();
        let focus_id = self
            .core
            .prev_pointer_window
            .and_then(|prev| self.core.xid_map.get(&prev).copied());
        if let (Some(focus), Some(grab)) = (focus_id, grab_id) {
            let events =
                yserver_core::crossings::implicit_grab_crossings(server_state, focus, grab);
            for ev in events {
                let win_host_xid = if ev.window == yserver_core::resources::ROOT_WINDOW {
                    server_state
                        .resources
                        .window(ev.window)
                        .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                        .unwrap_or(host_xid)
                } else {
                    self.crossing_host_for(server_state, ev.window, host_xid)
                };
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                self.emit_crossing(
                    origin,
                    win_host_xid,
                    kind,
                    ev.detail,
                    press_mode,
                    ev.child.0,
                    post_state,
                );
            }
        }
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_pointer_warp_pointer_root(
        &mut self,
        state: &mut ServerState,
        x: i32,
        y: i32,
    ) {
        // Route through the absolute-motion input path: updates the
        // tracked cursor (and HW cursor plane) and fans out the
        // motion/crossing events WarpPointer is specified to generate
        // ("as if the user had instantaneously moved the pointer").
        self.on_host_input(
            state,
            yserver_core::core_loop::HostInputEvent::PointerMotion {
                origin: yserver_core::core_loop::InputOrigin::NestedHost,
                x,
                y,
                time: 0,
                relative: false,
                dx: 0,
                dy: 0,
                motion_delta: None,
            },
        );
    }

    pub(in crate::kms::render::backend) fn backend_pointer_query_pointer(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<PointerPosition> {
        // Return the current core-tracked cursor position. No
        // window-focus lookup — Stage 1b doesn't model focus.
        //
        // The mask is a full X11 KeyButMask: live keyboard modifiers
        // (xkb state, low byte) | held buttons (0x100+). Xorg's
        // QueryPointer/XIQueryPointer report the paired keyboard's
        // modifier state here; pre-fix only buttons were included, so
        // cinnamon's alt-tab switcher (global.get_pointer() via
        // XIQueryPointer) read "Alt not held" mid-alt-tab and
        // instantly cancelled the popup.
        Ok(PointerPosition {
            same_screen: true,
            #[allow(clippy::cast_possible_truncation)]
            win_x: self.core.cursor_x as i16,
            #[allow(clippy::cast_possible_truncation)]
            win_y: self.core.cursor_y as i16,
            mask: self.core.button_mask | self.serialize_modifiers(),
        })
    }
}
