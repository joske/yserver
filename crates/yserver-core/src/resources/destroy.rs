//! One window destroy, decided once: [`ResourceTable::plan_destroy`] names
//! the windows, every side effect consumes the plan, and
//! [`ResourceTable::apply_destroy`] removes exactly them.

use std::{collections::HashSet, fmt::Write as _, io};

use yserver_protocol::x11::ResourceId;

use crate::resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_WINDOW, ResourceTable};

/// What one destroy will remove, decided by the resource table alone.
/// `windows` is Xorg `CrushTree` order (`dix/window.c:1023`): inferiors
/// first, topmost first, the subtree's top window last. Only
/// [`ResourceTable::plan_destroy`] makes one.
#[derive(Debug)]
pub struct DestroyPlan {
    /// `(window, parent)` as linked when planned.
    windows: Vec<(ResourceId, ResourceId)>,
    contains_cow: bool,
}

impl DestroyPlan {
    /// The window the destroy was asked for; last in [`Self::windows`].
    #[must_use]
    pub fn top(&self) -> ResourceId {
        self.windows.last().map(|(w, _)| *w).unwrap_or(ROOT_WINDOW)
    }

    /// The planned windows, CrushTree order.
    pub fn windows(&self) -> impl DoubleEndedIterator<Item = ResourceId> + '_ {
        self.windows.iter().map(|(w, _)| *w)
    }

    /// Whether the plan destroys the composite overlay window.
    #[must_use]
    pub fn contains_cow(&self) -> bool {
        self.contains_cow
    }

    /// Make the plan executable: a plan containing the COW runs the
    /// backend's fallible overlay release first.
    ///
    /// # Errors
    /// From `prepare_overlay_release`; nothing changed.
    pub fn prepare(self, backend: &mut dyn crate::backend::Backend) -> io::Result<PreparedDestroy> {
        let overlay = self.contains_cow && backend.prepare_overlay_release(None)?;
        Ok(PreparedDestroy {
            plan: self,
            overlay,
        })
    }
}

/// A [`DestroyPlan`] whose fallible preparation succeeded; the only input
/// the destroy executor takes.
#[derive(Debug)]
pub struct PreparedDestroy {
    plan: DestroyPlan,
    overlay: bool,
}

impl PreparedDestroy {
    #[must_use]
    pub fn plan(&self) -> &DestroyPlan {
        &self.plan
    }

    /// The backend owns the planned COW: its backend step is
    /// `retire_overlay_window`, not `destroy_subwindow`.
    #[must_use]
    pub fn retires_overlay(&self) -> bool {
        self.overlay
    }
}

impl ResourceTable {
    /// `None` if `id` is not a window or may not be destroyed (root: Xorg
    /// `ProcDestroyWindow` frees only a window with a parent,
    /// `dix/dispatch.c:829-835`).
    #[must_use]
    pub fn plan_destroy(&self, id: ResourceId) -> Option<DestroyPlan> {
        if id == ROOT_WINDOW {
            return None;
        }
        let parent = self.windows.get(&id.0)?.parent;
        let mut windows = Vec::new();
        self.crush_tree_order(id, parent, &mut windows);
        let contains_cow = windows.iter().any(|(w, _)| *w == COMPOSITE_OVERLAY_WINDOW);
        Some(DestroyPlan {
            windows,
            contains_cow,
        })
    }

    fn crush_tree_order(
        &self,
        id: ResourceId,
        parent: ResourceId,
        out: &mut Vec<(ResourceId, ResourceId)>,
    ) {
        let Some(w) = self.windows.get(&id.0) else {
            return;
        };
        // `children` is bottom-to-top; CrushTree starts at `firstChild`, the topmost.
        for child in w.children.iter().rev() {
            self.crush_tree_order(*child, id, out);
        }
        out.push((id, parent));
    }

    /// Remove exactly `plan`'s windows. Decides nothing.
    ///
    /// # Panics
    /// On a stale plan: a planned window is gone or relinked, or a planned
    /// window has a child the plan does not name.
    pub fn apply_destroy(&mut self, plan: &DestroyPlan) {
        if let Some(problem) = self.stale_plan_problem(plan) {
            panic!(
                "apply_destroy: stale plan ({problem}); plan {:x?}; actual {}",
                plan.windows,
                self.describe_planned(plan)
            );
        }
        let (top, top_parent) = *plan.windows.last().expect("a plan has its top window");
        if let Some(parent) = self.windows.get_mut(&top_parent.0) {
            parent.children.retain(|child| *child != top);
        }
        for (id, _) in &plan.windows {
            let window = self.windows.remove(&id.0).expect("checked above");
            if let Some(host) = window.cursor_host {
                self.dropped_cursor_hosts.push(host.as_raw());
            }
        }
    }

    fn stale_plan_problem(&self, plan: &DestroyPlan) -> Option<String> {
        let planned: HashSet<ResourceId> = plan.windows().collect();
        let Some(&(top, top_parent)) = plan.windows.last() else {
            return Some("empty plan".to_owned());
        };
        if !self
            .windows
            .get(&top_parent.0)
            .is_some_and(|p| p.children.contains(&top))
        {
            return Some(format!(
                "parent 0x{:x} no longer lists top 0x{:x}",
                top_parent.0, top.0
            ));
        }
        for (id, parent) in &plan.windows {
            let Some(w) = self.windows.get(&id.0) else {
                return Some(format!("0x{:x} is gone", id.0));
            };
            if w.parent != *parent {
                return Some(format!("0x{:x} has parent 0x{:x}", id.0, w.parent.0));
            }
            if let Some(child) = w.children.iter().find(|c| !planned.contains(c)) {
                return Some(format!("0x{:x} has unplanned child 0x{:x}", id.0, child.0));
            }
        }
        None
    }

    fn describe_planned(&self, plan: &DestroyPlan) -> String {
        let mut out = String::new();
        for (id, _) in &plan.windows {
            match self.windows.get(&id.0) {
                Some(w) => {
                    let _ = write!(
                        out,
                        "[0x{:x} parent 0x{:x} children {:x?}] ",
                        id.0, w.parent.0, w.children
                    );
                }
                None => {
                    let _ = write!(out, "[0x{:x} gone] ", id.0);
                }
            }
        }
        out
    }
}
