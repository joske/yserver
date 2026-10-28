//! The root's children → top-level lifecycle events (spec §4.1, §4.2).

use std::num::NonZeroU64;

use yserver_protocol::x11::{ResourceId, xfixes::RegionRect};

use super::{
    Rootless, WindowInfo,
    acks::Acks,
    info::{parse_window_info, transient_for_window},
    types::{Geometry, GeometryFields, Rect, RootlessEvent, ToplevelId, ToplevelKind},
};
use crate::{
    resources::{
        COMPOSITE_OVERLAY_WINDOW, MapState, ROOT_WINDOW, ROOTLESS_WM_CHECK_WINDOW,
        SCREEN_SAVER_WINDOW, WindowClass,
    },
    server::ServerState,
};

/// What the host was last told about one mapped top-level.
#[derive(Clone, Debug)]
pub(super) struct Tracked {
    pub id: ToplevelId,
    pub kind: ToplevelKind,
    pub geometry: Geometry,
    pub bounding: Option<Vec<Rect>>,
    pub input: Option<Vec<Rect>>,
    pub info: WindowInfo,
    pub acks: Acks,
}

/// Spec §4.1 predicate: a viewable InputOutput root child that is not a
/// server window (COW, screensaver).
pub(super) fn is_toplevel(state: &ServerState, window: ResourceId) -> bool {
    if window == COMPOSITE_OVERLAY_WINDOW
        || window == SCREEN_SAVER_WINDOW
        || window == ROOTLESS_WM_CHECK_WINDOW
    {
        return false;
    }
    state.resources.window(window).is_some_and(|w| {
        w.parent == ROOT_WINDOW
            && w.map_state == MapState::Viewable
            && w.class != WindowClass::InputOnly
    })
}

fn geometry_of(state: &ServerState, window: ResourceId) -> Option<Geometry> {
    state.resources.window(window).map(|w| Geometry {
        x: i32::from(w.x),
        y: i32::from(w.y),
        width: w.width,
        height: w.height,
        border: w.border_width,
    })
}

fn changed_fields(old: Geometry, new: Geometry) -> GeometryFields {
    let mut f = GeometryFields::empty();
    for (changed, field) in [
        (old.x != new.x, GeometryFields::X),
        (old.y != new.y, GeometryFields::Y),
        (old.width != new.width, GeometryFields::WIDTH),
        (old.height != new.height, GeometryFields::HEIGHT),
        (old.border != new.border, GeometryFields::BORDER),
    ] {
        if changed {
            f |= field;
        }
    }
    f
}

/// Shape rects (content-origin X coordinates) in backing pixels: `+bw`
/// (spec §4.0; `dix/window.c:1735-1742`, `compalloc.c:600-606`).
fn to_backing(rects: Option<&Vec<RegionRect>>, bw: u16) -> Option<Vec<Rect>> {
    rects.map(|rects| {
        rects
            .iter()
            .map(|r| Rect {
                x: i32::from(r.x) + i32::from(bw),
                y: i32::from(r.y) + i32::from(bw),
                width: r.width,
                height: r.height,
            })
            .collect()
    })
}

fn shapes_of(
    state: &ServerState,
    window: ResourceId,
    bw: u16,
) -> (Option<Vec<Rect>>, Option<Vec<Rect>>) {
    state.shape_windows.get(&window).map_or((None, None), |s| {
        (to_backing(s.bounding(), bw), to_backing(s.input(), bw))
    })
}

/// Indices into `old_pos` (old stacking index per survivor, in new order)
/// of a longest increasing run, preferring lower windows on ties. Survivors
/// outside it are the ones that moved.
fn kept_in_place(old_pos: &[usize]) -> Vec<bool> {
    let n = old_pos.len();
    let mut len = vec![1usize; n];
    let mut prev = vec![None; n];
    for i in 0..n {
        for j in 0..i {
            if old_pos[j] < old_pos[i] && len[j] + 1 > len[i] {
                len[i] = len[j] + 1;
                prev[i] = Some(j);
            }
        }
    }
    let mut kept = vec![false; n];
    let mut at = (0..n).fold(None, |best: Option<usize>, i| match best {
        Some(b) if len[b] >= len[i] => Some(b),
        _ => Some(i),
    });
    while let Some(i) = at {
        kept[i] = true;
        at = prev[i];
    }
    kept
}

impl Rootless {
    pub(super) fn allocate_id(&mut self) -> ToplevelId {
        let id = ToplevelId::new(self.next_id);
        // 2^64 mappings never happen; wrap rather than panic.
        self.next_id = self.next_id.checked_add(1).unwrap_or(NonZeroU64::MIN);
        id
    }

    fn retire(&mut self, window: ResourceId, events: &mut Vec<RootlessEvent>, destroyed: bool) {
        if let Some(t) = self.toplevels.remove(&window) {
            self.live.remove(&t.id);
            if t.kind == ToplevelKind::Managed {
                self.withdrawn.push(window);
            }
            events.push(RootlessEvent::Unmapped { id: t.id });
            if destroyed {
                events.push(RootlessEvent::Destroyed { id: t.id });
            }
        }
    }

    /// Popup parent (spec §10 order, rungs implemented so far): the Managed
    /// `WM_TRANSIENT_FOR` names, else the topmost Managed whose outer rect
    /// contains the popup's outer origin, else the topmost Managed, else none.
    fn popup_parent(
        &self,
        order: &[ResourceId],
        popup: Geometry,
        transient_for: Option<ResourceId>,
    ) -> Option<ToplevelId> {
        if let Some(t) = transient_for
            .and_then(|w| self.toplevels.get(&w))
            .filter(|t| t.kind == ToplevelKind::Managed)
        {
            return Some(t.id);
        }
        if let super::PointerScope::Within(w) = self.scope
            && let Some(t) = self
                .toplevels
                .get(&w)
                .filter(|t| t.kind == ToplevelKind::Managed)
        {
            return Some(t.id);
        }
        let managed: Vec<&Tracked> = order
            .iter()
            .rev()
            .filter_map(|w| self.toplevels.get(w))
            .filter(|t| t.kind == ToplevelKind::Managed)
            .collect();
        let contains = |t: &Tracked| {
            let g = t.geometry;
            let (w, h) = (
                i32::from(g.width) + 2 * i32::from(g.border),
                i32::from(g.height) + 2 * i32::from(g.border),
            );
            popup.x >= g.x && popup.y >= g.y && popup.x < g.x + w && popup.y < g.y + h
        };
        managed
            .iter()
            .find(|t| contains(t))
            .or_else(|| managed.first())
            .map(|t| t.id)
    }

    /// Diff the root's children against what the host was told (spec §4.1).
    pub(super) fn reconcile_topology(&mut self, state: &ServerState) -> Vec<RootlessEvent> {
        let mut events = Vec::new();

        // 1-2. Gone, reparented away, or no longer viewable.
        let old_order = std::mem::take(&mut self.order);
        for w in &old_order {
            let still_child = state
                .resources
                .window(*w)
                .is_some_and(|win| win.parent == ROOT_WINDOW);
            if !still_child {
                self.retire(*w, &mut events, true);
            } else if !is_toplevel(state, *w) {
                self.retire(*w, &mut events, false);
            }
        }

        // The new stacking, bottom → top.
        let order: Vec<ResourceId> = state
            .resources
            .children(ROOT_WINDOW)
            .iter()
            .copied()
            .filter(|w| is_toplevel(state, *w))
            .collect();

        // Survivors that moved relative to each other (5).
        let survivors: Vec<ResourceId> = order
            .iter()
            .copied()
            .filter(|w| self.toplevels.contains_key(w))
            .collect();
        // A window a request restacked is reported; of the rest, those
        // outside the longest unchanged run moved by side effect.
        let requested = |w: &ResourceId| self.toplevels[w].acks.restacked;
        let others: Vec<ResourceId> = survivors
            .iter()
            .copied()
            .filter(|w| !requested(w))
            .collect();
        let old_pos: Vec<usize> = others
            .iter()
            .map(|w| old_order.iter().position(|o| o == w).unwrap_or(0))
            .collect();
        let kept = kept_in_place(&old_pos);
        let moved: Vec<ResourceId> = survivors
            .iter()
            .copied()
            .filter(|w| requested(w) || others.iter().zip(&kept).any(|(o, k)| o == w && !*k))
            .collect();

        let mut mapped_now = Vec::new();
        // 3. New top-levels, bottom → top, above the nearest lower window
        // that does not move this iteration.
        for (i, w) in order.iter().enumerate() {
            if self.toplevels.contains_key(w) {
                continue;
            }
            let Some(win) = state.resources.window(*w) else {
                continue;
            };
            let Some(geometry) = geometry_of(state, *w) else {
                continue;
            };
            let kind = if win.override_redirect {
                ToplevelKind::Popup {
                    parent: self.popup_parent(
                        &order[..i],
                        geometry,
                        transient_for_window(state, *w),
                    ),
                }
            } else {
                ToplevelKind::Managed
            };
            let above = order[..i]
                .iter()
                .rev()
                .filter(|o| !moved.contains(o))
                .find_map(|o| self.toplevels.get(o).map(|t| t.id));
            let info = parse_window_info(state, *w, &|x| self.toplevels.get(&x).map(|t| t.id));
            let id = self.allocate_id();
            self.live.insert(id, *w);
            mapped_now.push(*w);
            if kind == ToplevelKind::Managed {
                self.client_list.push(*w);
            }
            self.toplevels.insert(
                *w,
                Tracked {
                    id,
                    kind,
                    geometry,
                    bounding: None,
                    input: None,
                    info: info.clone(),
                    acks: Acks::default(),
                },
            );
            events.push(RootlessEvent::Mapped {
                id,
                window: *w,
                kind,
                geometry,
                depth: win.depth,
                info,
                above,
            });
        }

        // 4. Geometry.
        for w in &survivors {
            let Some(new) = geometry_of(state, *w) else {
                continue;
            };
            let t = self.toplevels.get_mut(w).expect("survivor is tracked");
            let changed = changed_fields(t.geometry, new);
            let unexplained = changed.without(t.acks.client_fields | t.acks.host_fields);
            if !unexplained.is_empty() {
                log::debug!(
                    "rootless: 0x{:x} geometry {unexplained:?} changed outside ConfigureWindow",
                    w.0
                );
                t.acks.client_fields |= unexplained;
            }
            if !changed.is_empty()
                || t.acks.geometry_acked.is_some()
                || !t.acks.client_fields.is_empty()
            {
                t.geometry = new;
                events.push(RootlessEvent::Configured {
                    id: t.id,
                    geometry: new,
                    acked: t.acks.geometry_acked,
                    client_fields: t.acks.client_fields,
                });
            }
            t.acks.take_geometry();
        }

        // 5. Stacking, bottom → top.
        for (i, w) in order.iter().enumerate() {
            if !moved.contains(w) {
                continue;
            }
            let above = order[..i]
                .iter()
                .rev()
                .find_map(|o| self.toplevels.get(o).map(|t| t.id));
            let t = self.toplevels.get_mut(w).expect("survivor is tracked");
            events.push(RootlessEvent::Restacked {
                id: t.id,
                above,
                acked: t.acks.stack_acked,
                // Rule 6; a move no request explains counts as a client one.
                from_client: t.acks.from_client || t.acks.stack_acked.is_none(),
            });
        }
        for t in self.toplevels.values_mut() {
            t.acks.take_stack();
        }

        // 6. Shapes.
        for w in &order {
            let t = self.toplevels.get_mut(w).expect("tracked above");
            let (bounding, input) = shapes_of(state, *w, t.geometry.border);
            if bounding != t.bounding || input != t.input {
                t.bounding.clone_from(&bounding);
                t.input.clone_from(&input);
                events.push(RootlessEvent::Reshaped {
                    id: t.id,
                    bounding,
                    input,
                });
            }
        }

        // Window info (spec §4.3): re-parsed when its properties changed; the
        // transient parent re-resolved every time, since it may have mapped
        // or gone away. A popup follows with its placement parent.
        let dirty = std::mem::take(&mut self.dirty_props);
        for (i, w) in order.iter().enumerate() {
            let resolve = |x: ResourceId| self.toplevels.get(&x).map(|t| t.id);
            let t = &self.toplevels[w];
            let info = if dirty.contains(w) && !mapped_now.contains(w) {
                parse_window_info(state, *w, &resolve)
            } else {
                WindowInfo {
                    transient_for: transient_for_window(state, *w).and_then(resolve),
                    ..t.info.clone()
                }
            };
            let parent = match t.kind {
                ToplevelKind::Popup { parent } => {
                    let transient = transient_for_window(state, *w)
                        .and_then(|x| self.toplevels.get(&x))
                        .filter(|p| p.kind == ToplevelKind::Managed)
                        .map(|p| p.id);
                    let next = transient.or_else(|| {
                        parent
                            .filter(|p| self.live.contains_key(p))
                            .map_or_else(|| self.popup_parent(&order[..i], t.geometry, None), Some)
                    });
                    (next != parent).then_some(next)
                }
                ToplevelKind::Managed => None,
            };
            let t = self.toplevels.get_mut(w).expect("tracked above");
            let changed = info.changed_from(&t.info);
            if !changed.is_empty() {
                t.info.clone_from(&info);
                events.push(RootlessEvent::InfoChanged {
                    id: t.id,
                    info,
                    changed,
                });
            }
            if let Some(parent) = parent {
                t.kind = ToplevelKind::Popup { parent };
                events.push(RootlessEvent::ParentChanged { id: t.id, parent });
            }
        }

        // 7. Client requests to the WM (spec §6.3), for mapped windows only.
        for (w, request) in std::mem::take(&mut self.pending_wm) {
            match self.toplevels.get(&w) {
                Some(t) => events.push(RootlessEvent::WmRequest { id: t.id, request }),
                None => log::debug!("rootless: {request:?} for unmapped 0x{:x} dropped", w.0),
            }
        }

        self.order = order;
        events
    }
}
