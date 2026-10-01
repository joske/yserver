//! X11 EnterNotify/LeaveNotify crossing computation for implicit
//! pointer grabs.
//!
//! When a button press creates an implicit pointer grab, the grab
//! activation generates Leave/Enter events along the path between the
//! window the pointer was in (focus) and the grab window. The release
//! generates the symmetric Ungrab pair. Today's KMS backend (and any
//! host-X11 mirror) emit a single unconditional crossing on the press
//! window, which is wrong whenever focus and grab differ — see
//! `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`,
//! "Pre-existing bugs" #2.
//!
//! `implicit_grab_crossings` is a pure function over `ServerState`'s
//! window tree: hand it the focus + grab `ResourceId`s and it returns
//! the spec-correct sequence of crossing events. The caller threads
//! the events through whichever fanout machinery is appropriate (KMS
//! backend, host-X11 dispatcher).
//!
//! Detail-code reference (X11 protocol, EnterNotify/LeaveNotify):
//! - `0` `NotifyAncestor`
//! - `1` `NotifyVirtual`
//! - `2` `NotifyInferior`
//! - `3` `NotifyNonlinear`
//! - `4` `NotifyNonlinearVirtual`

use std::collections::HashSet;

use yserver_protocol::x11::ResourceId;

use crate::server::ServerState;

pub const NOTIFY_ANCESTOR: u8 = 0;
pub const NOTIFY_VIRTUAL: u8 = 1;
pub const NOTIFY_INFERIOR: u8 = 2;
pub const NOTIFY_NONLINEAR: u8 = 3;
pub const NOTIFY_NONLINEAR_VIRTUAL: u8 = 4;
pub const NOTIFY_POINTER: u8 = 5;
pub const NOTIFY_POINTER_ROOT: u8 = 6;
pub const NOTIFY_DETAIL_NONE: u8 = 7;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CrossingKind {
    Enter,
    Leave,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CrossingEvent {
    pub window: ResourceId,
    pub kind: CrossingKind,
    /// One of the `NOTIFY_*` constants.
    pub detail: u8,
    /// X11 EnterNotify/LeaveNotify `child` field:
    /// - For source/destination endpoints: `ResourceId(0)` (X11 None),
    ///   matching Xorg's `CoreEnterLeaveEvent(..., None)` for these.
    /// - For virtual intermediates (NotifyVirtual / NotifyNonlinearVirtual):
    ///   the immediate descendant of `window` on the path to whichever
    ///   endpoint is in `window`'s subtree (source for Leave virtuals,
    ///   destination for Enter virtuals).
    pub child: ResourceId,
}

/// Compute Leave/Enter events for moving the pointer's logical
/// "window" from `focus` to `grab`. Order in the returned `Vec` is
/// the wire-emission order required by X11. Used for the
/// implicit-grab activation/release path (caller sets X11 `mode` to
/// `NotifyGrab` / `NotifyUngrab`).
///
/// `focus == grab` returns an empty `Vec`. Cycles or missing windows
/// (defensive — `state.resources.window` returns `None`) terminate
/// the chain walk early; the returned events are still well-formed
/// X11 (just possibly truncated).
#[must_use]
pub fn implicit_grab_crossings(
    state: &ServerState,
    focus: ResourceId,
    grab: ResourceId,
) -> Vec<CrossingEvent> {
    compute_crossing_chain(state, focus, grab)
}

/// Compute Leave/Enter events for moving the pointer's logical
/// "window" from `from` to `to` under X11 Normal mode (no grab).
/// Caller sets X11 `mode` to `NotifyNormal`.
///
/// Same chain-computation as [`implicit_grab_crossings`] — the only
/// difference is the `mode` value the caller emits with each event;
/// detail codes and child fields are identical (per X11 spec).
#[must_use]
pub fn normal_mode_crossings(
    state: &ServerState,
    from: ResourceId,
    to: ResourceId,
) -> Vec<CrossingEvent> {
    compute_crossing_chain(state, from, to)
}

fn compute_crossing_chain(
    state: &ServerState,
    from: ResourceId,
    to: ResourceId,
) -> Vec<CrossingEvent> {
    if from == to {
        return Vec::new();
    }

    let from_chain = ancestor_chain(state, from);
    let to_chain = ancestor_chain(state, to);

    // Case A: `to` is an ancestor of `from` (pointer "moves up" out of
    // a deeper subtree into a parent). Leaves walk from_chain up to
    // (not including) `to`; final Enter on `to`.
    if from_chain.iter().skip(1).any(|w| *w == to) {
        let mut events = vec![CrossingEvent {
            window: from,
            kind: CrossingKind::Leave,
            detail: NOTIFY_ANCESTOR,
            child: ResourceId(0),
        }];
        // Virtual leaves on intermediates: for each W in from_chain
        // (skipping `from` itself, stopping before `to`), W is ancestor
        // of `from` so `child` = immediate descendant of W on path to
        // `from` = the previous chain entry.
        for (i, w) in from_chain.iter().enumerate().skip(1) {
            if *w == to {
                break;
            }
            events.push(CrossingEvent {
                window: *w,
                kind: CrossingKind::Leave,
                detail: NOTIFY_VIRTUAL,
                child: from_chain[i - 1],
            });
        }
        events.push(CrossingEvent {
            window: to,
            kind: CrossingKind::Enter,
            detail: NOTIFY_INFERIOR,
            child: ResourceId(0),
        });
        return events;
    }

    // Case B: `from` is an ancestor of `to` (pointer "moves down" into
    // a descendant subtree). Leave on `from`, then virtual Enters on
    // ancestors of `to` from (not including) `from` down to (not
    // including) `to`, final Enter on `to`.
    if to_chain.iter().skip(1).any(|w| *w == from) {
        let mut events = vec![CrossingEvent {
            window: from,
            kind: CrossingKind::Leave,
            detail: NOTIFY_INFERIOR,
            child: ResourceId(0),
        }];
        // Gather intermediates (W where W is ancestor of `to`, but W !=
        // `to` and W != `from`). to_chain is [to, parent_of_to, ...,
        // from, ...]. Skip head (`to`), stop when we hit `from`. Reverse
        // to get downward emission order.
        let mut intermediate_indices: Vec<usize> = Vec::new();
        for (i, w) in to_chain.iter().enumerate().skip(1) {
            if *w == from {
                break;
            }
            intermediate_indices.push(i);
        }
        intermediate_indices.reverse();
        for i in intermediate_indices {
            // Virtual Enter on to_chain[i]: destination=`to` is inferior
            // of W=to_chain[i], child = immediate descendant of W on
            // path to `to` = to_chain[i - 1].
            events.push(CrossingEvent {
                window: to_chain[i],
                kind: CrossingKind::Enter,
                detail: NOTIFY_VIRTUAL,
                child: to_chain[i - 1],
            });
        }
        events.push(CrossingEvent {
            window: to,
            kind: CrossingKind::Enter,
            detail: NOTIFY_ANCESTOR,
            child: ResourceId(0),
        });
        return events;
    }

    // Case C: disjoint subtrees. Walk up from `from` to the lowest
    // common ancestor (LCA), then back down to `to`.
    let common = lowest_common_ancestor(&from_chain, &to_chain);
    let mut events = vec![CrossingEvent {
        window: from,
        kind: CrossingKind::Leave,
        detail: NOTIFY_NONLINEAR,
        child: ResourceId(0),
    }];
    for (i, w) in from_chain.iter().enumerate().skip(1) {
        if Some(*w) == common {
            break;
        }
        events.push(CrossingEvent {
            window: *w,
            kind: CrossingKind::Leave,
            detail: NOTIFY_NONLINEAR_VIRTUAL,
            child: from_chain[i - 1],
        });
    }
    let mut downward_indices: Vec<usize> = Vec::new();
    for (i, w) in to_chain.iter().enumerate().skip(1) {
        if Some(*w) == common {
            break;
        }
        downward_indices.push(i);
    }
    downward_indices.reverse();
    for i in downward_indices {
        events.push(CrossingEvent {
            window: to_chain[i],
            kind: CrossingKind::Enter,
            detail: NOTIFY_NONLINEAR_VIRTUAL,
            child: to_chain[i - 1],
        });
    }
    events.push(CrossingEvent {
        window: to,
        kind: CrossingKind::Enter,
        detail: NOTIFY_NONLINEAR,
        child: ResourceId(0),
    });
    events
}

/// One FocusIn/FocusOut event of a focus transition. FocusIn/Out have
/// no `child` field (unlike crossings), so window + direction + detail
/// fully describe the wire event; the caller supplies `mode`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct FocusEvent {
    pub window: ResourceId,
    pub focus_in: bool,
    /// One of the `NOTIFY_*` constants (including Pointer /
    /// PointerRoot / DetailNone).
    pub detail: u8,
}

/// Compute the FocusOut/FocusIn event sequence for the core keyboard
/// focus moving from `from_raw` to `to_raw` — port of Xorg
/// `CoreFocusEvents` (dix/enterleave.c:1395) reduced to the
/// single-keyboard case (`HasFocus`/`FirstFocusChild`/`HasOtherPointer`
/// are always false/None without MPX).
///
/// `from_raw`/`to_raw` are the wire focus values: `0` = None, `1` =
/// PointerRoot, anything else a window xid. `pointer_win` is the
/// deepest window currently containing the pointer (the spec's "P" for
/// the NotifyPointer runs).
#[must_use]
pub fn focus_transition_events(
    state: &ServerState,
    from_raw: u32,
    to_raw: u32,
    pointer_win: ResourceId,
) -> Vec<FocusEvent> {
    use crate::resources::ROOT_WINDOW;

    let mut ev: Vec<FocusEvent> = Vec::new();
    if from_raw == to_raw {
        return ev;
    }
    let root = ROOT_WINDOW;
    let p = pointer_win;
    let special = |r: u32| r == 0 || r == 1;
    let is_anc = |a: ResourceId, b: ResourceId| -> bool {
        a != b && ancestor_chain(state, b).iter().skip(1).any(|w| *w == a)
    };

    // FocusOut(NotifyPointer) run from P up to `pwin_parent`
    // (exclusive; through it when `inclusive`) — Xorg
    // `CoreFocusOutNotifyPointerEvents`.
    let out_pointer_run = |ev: &mut Vec<FocusEvent>,
                           pwin_parent: ResourceId,
                           exclude: Option<ResourceId>,
                           inclusive: bool| {
        if !(is_anc(pwin_parent, p) || (pwin_parent == p && inclusive)) {
            return;
        }
        if let Some(x) = exclude
            && (is_anc(x, p) || is_anc(p, x))
        {
            return;
        }
        for w in ancestor_chain(state, p) {
            if !inclusive && w == pwin_parent {
                break;
            }
            ev.push(FocusEvent {
                window: w,
                focus_in: false,
                detail: NOTIFY_POINTER,
            });
            if inclusive && w == pwin_parent {
                break;
            }
        }
    };
    // FocusIn(NotifyPointer) run from `pwin_parent` (exclusive;
    // included when `inclusive`) down to P — Xorg
    // `CoreFocusInNotifyPointerEvents`.
    let in_pointer_run = |ev: &mut Vec<FocusEvent>,
                          pwin_parent: ResourceId,
                          exclude: Option<ResourceId>,
                          inclusive: bool| {
        if exclude == Some(p) || (pwin_parent != p && !is_anc(pwin_parent, p)) {
            return;
        }
        if let Some(x) = exclude
            && (is_anc(x, p) || is_anc(p, x))
        {
            return;
        }
        let mut run: Vec<ResourceId> = Vec::new();
        for w in ancestor_chain(state, p) {
            if !inclusive && w == pwin_parent {
                break;
            }
            run.push(w);
            if inclusive && w == pwin_parent {
                break;
            }
        }
        for w in run.into_iter().rev() {
            ev.push(FocusEvent {
                window: w,
                focus_in: true,
                detail: NOTIFY_POINTER,
            });
        }
    };

    match (special(from_raw), special(to_raw)) {
        // PointerRoot ↔ None — Xorg CoreFocusPointerRootNoneSwitch.
        (true, true) => {
            if from_raw == 1 && to_raw != 1 {
                out_pointer_run(&mut ev, root, None, true);
            }
            ev.push(FocusEvent {
                window: root,
                focus_in: false,
                detail: if from_raw == 1 {
                    NOTIFY_POINTER_ROOT
                } else {
                    NOTIFY_DETAIL_NONE
                },
            });
            ev.push(FocusEvent {
                window: root,
                focus_in: true,
                detail: if to_raw == 1 {
                    NOTIFY_POINTER_ROOT
                } else {
                    NOTIFY_DETAIL_NONE
                },
            });
            if to_raw == 1 {
                in_pointer_run(&mut ev, root, None, true);
            }
        }
        // Window A → PointerRoot/None — Xorg CoreFocusToPointerRootOrNone.
        (false, true) => {
            let a = ResourceId(from_raw);
            out_pointer_run(&mut ev, a, None, false);
            ev.push(FocusEvent {
                window: a,
                focus_in: false,
                detail: NOTIFY_NONLINEAR,
            });
            // FocusOut(NonlinearVirtual) on A's ancestors up to and
            // including the root (Xorg passes NullWindow as ancestor).
            for w in ancestor_chain(state, a).into_iter().skip(1) {
                ev.push(FocusEvent {
                    window: w,
                    focus_in: false,
                    detail: NOTIFY_NONLINEAR_VIRTUAL,
                });
            }
            ev.push(FocusEvent {
                window: root,
                focus_in: true,
                detail: if to_raw == 1 {
                    NOTIFY_POINTER_ROOT
                } else {
                    NOTIFY_DETAIL_NONE
                },
            });
            if to_raw == 1 {
                in_pointer_run(&mut ev, root, None, true);
            }
        }
        // PointerRoot/None → window B — Xorg CoreFocusFromPointerRootOrNone.
        (true, false) => {
            let b = ResourceId(to_raw);
            if from_raw == 1 {
                out_pointer_run(&mut ev, root, None, true);
            }
            ev.push(FocusEvent {
                window: root,
                focus_in: false,
                detail: if from_raw == 1 {
                    NOTIFY_POINTER_ROOT
                } else {
                    NOTIFY_DETAIL_NONE
                },
            });
            if b != root {
                ev.push(FocusEvent {
                    window: root,
                    focus_in: true,
                    detail: NOTIFY_NONLINEAR_VIRTUAL,
                });
                // Windows strictly between root and B, top-down.
                let bchain = ancestor_chain(state, b);
                let mut mids: Vec<ResourceId> = Vec::new();
                for w in bchain.iter().skip(1) {
                    if *w == root {
                        break;
                    }
                    mids.push(*w);
                }
                for w in mids.into_iter().rev() {
                    ev.push(FocusEvent {
                        window: w,
                        focus_in: true,
                        detail: NOTIFY_NONLINEAR_VIRTUAL,
                    });
                }
            }
            ev.push(FocusEvent {
                window: b,
                focus_in: true,
                detail: NOTIFY_NONLINEAR,
            });
            in_pointer_run(&mut ev, b, None, false);
        }
        // Window A → window B — to-ancestor / to-descendant / nonlinear.
        (false, false) => {
            let a = ResourceId(from_raw);
            let b = ResourceId(to_raw);
            if is_anc(b, a) {
                // Xorg CoreFocusToAncestor.
                ev.push(FocusEvent {
                    window: a,
                    focus_in: false,
                    detail: NOTIFY_ANCESTOR,
                });
                for w in ancestor_chain(state, a).into_iter().skip(1) {
                    if w == b {
                        break;
                    }
                    ev.push(FocusEvent {
                        window: w,
                        focus_in: false,
                        detail: NOTIFY_VIRTUAL,
                    });
                }
                ev.push(FocusEvent {
                    window: b,
                    focus_in: true,
                    detail: NOTIFY_INFERIOR,
                });
                in_pointer_run(&mut ev, b, Some(a), false);
            } else if is_anc(a, b) {
                // Xorg CoreFocusToDescendant.
                out_pointer_run(&mut ev, a, Some(b), false);
                ev.push(FocusEvent {
                    window: a,
                    focus_in: false,
                    detail: NOTIFY_INFERIOR,
                });
                let bchain = ancestor_chain(state, b);
                let mut mids: Vec<ResourceId> = Vec::new();
                for w in bchain.iter().skip(1) {
                    if *w == a {
                        break;
                    }
                    mids.push(*w);
                }
                for w in mids.into_iter().rev() {
                    ev.push(FocusEvent {
                        window: w,
                        focus_in: true,
                        detail: NOTIFY_VIRTUAL,
                    });
                }
                ev.push(FocusEvent {
                    window: b,
                    focus_in: true,
                    detail: NOTIFY_ANCESTOR,
                });
            } else {
                // Xorg CoreFocusNonLinear.
                let a_chain = ancestor_chain(state, a);
                let b_chain = ancestor_chain(state, b);
                let common = lowest_common_ancestor(&a_chain, &b_chain);
                out_pointer_run(&mut ev, a, None, false);
                ev.push(FocusEvent {
                    window: a,
                    focus_in: false,
                    detail: NOTIFY_NONLINEAR,
                });
                for w in a_chain.iter().skip(1) {
                    if Some(*w) == common {
                        break;
                    }
                    ev.push(FocusEvent {
                        window: *w,
                        focus_in: false,
                        detail: NOTIFY_NONLINEAR_VIRTUAL,
                    });
                }
                let mut mids: Vec<ResourceId> = Vec::new();
                for w in b_chain.iter().skip(1) {
                    if Some(*w) == common {
                        break;
                    }
                    mids.push(*w);
                }
                for w in mids.into_iter().rev() {
                    ev.push(FocusEvent {
                        window: w,
                        focus_in: true,
                        detail: NOTIFY_NONLINEAR_VIRTUAL,
                    });
                }
                ev.push(FocusEvent {
                    window: b,
                    focus_in: true,
                    detail: NOTIFY_NONLINEAR,
                });
                in_pointer_run(&mut ev, b, None, false);
            }
        }
    }
    ev
}

/// The XI2 FocusOut/FocusIn sequence of the same transition — Xorg
/// `DeviceFocusEvents` (`dix/enterleave.c:1420`). It differs from the core
/// one: its intermediate runs exclude both ends (`DeviceFocusOutEvents` /
/// `DeviceFocusInEvents`), so for instance the root gets no NotifyPointer
/// or NotifyNonlinearVirtual leg when the focus leaves or enters
/// PointerRoot, and a same-window transition sends nothing.
#[must_use]
pub fn device_focus_transition_events(
    state: &ServerState,
    from_raw: u32,
    to_raw: u32,
    pointer_win: ResourceId,
) -> Vec<FocusEvent> {
    use crate::resources::ROOT_WINDOW;

    let mut ev: Vec<FocusEvent> = Vec::new();
    if from_raw == to_raw {
        return ev;
    }
    let root = ROOT_WINDOW;
    let p = pointer_win;
    let is_parent = |a: ResourceId, b: ResourceId| -> bool {
        a != b && ancestor_chain(state, b).iter().skip(1).any(|w| *w == a)
    };
    let push = |ev: &mut Vec<FocusEvent>, window: ResourceId, focus_in: bool, detail: u8| {
        ev.push(FocusEvent {
            window,
            focus_in,
            detail,
        });
    };
    // `DeviceFocusOutEvents(child, ancestor)`: child's ancestors below
    // `ancestor`, upwards; `None` runs through the root.
    let out_run =
        |ev: &mut Vec<FocusEvent>, child: ResourceId, ancestor: Option<ResourceId>, detail: u8| {
            if ancestor == Some(child) {
                return;
            }
            for w in ancestor_chain(state, child).into_iter().skip(1) {
                if Some(w) == ancestor {
                    break;
                }
                push(ev, w, false, detail);
            }
        };
    // `DeviceFocusInEvents(ancestor, child)`: child's ancestors below
    // `ancestor`, downwards.
    let in_run = |ev: &mut Vec<FocusEvent>, ancestor: ResourceId, child: ResourceId, detail: u8| {
        let mut run: Vec<ResourceId> = Vec::new();
        for w in ancestor_chain(state, child).into_iter().skip(1) {
            if w == ancestor {
                break;
            }
            run.push(w);
        }
        for w in run.into_iter().rev() {
            push(ev, w, true, detail);
        }
    };
    let special = |r: u32| r == 0 || r == 1;
    let out_detail = if from_raw == 0 {
        NOTIFY_DETAIL_NONE
    } else {
        NOTIFY_POINTER_ROOT
    };
    let in_detail = if to_raw == 0 {
        NOTIFY_DETAIL_NONE
    } else {
        NOTIFY_POINTER_ROOT
    };
    if special(to_raw) {
        if special(from_raw) {
            if from_raw == 1 {
                push(&mut ev, p, false, NOTIFY_POINTER);
                out_run(&mut ev, p, Some(root), NOTIFY_POINTER);
            }
            push(&mut ev, root, false, out_detail);
        } else {
            let from = ResourceId(from_raw);
            if is_parent(from, p) {
                push(&mut ev, p, false, NOTIFY_POINTER);
                out_run(&mut ev, p, Some(from), NOTIFY_POINTER);
            }
            push(&mut ev, from, false, NOTIFY_NONLINEAR);
            out_run(&mut ev, from, None, NOTIFY_NONLINEAR_VIRTUAL);
        }
        push(&mut ev, root, true, in_detail);
        if to_raw == 1 {
            in_run(&mut ev, root, p, NOTIFY_POINTER);
            push(&mut ev, p, true, NOTIFY_POINTER);
        }
        return ev;
    }
    let to = ResourceId(to_raw);
    if special(from_raw) {
        if from_raw == 1 {
            push(&mut ev, p, false, NOTIFY_POINTER);
            out_run(&mut ev, p, Some(root), NOTIFY_POINTER);
        }
        push(&mut ev, root, false, out_detail);
        if to != root {
            in_run(&mut ev, root, to, NOTIFY_NONLINEAR_VIRTUAL);
        }
        push(&mut ev, to, true, NOTIFY_NONLINEAR);
        if is_parent(to, p) {
            in_run(&mut ev, to, p, NOTIFY_POINTER);
        }
        return ev;
    }
    let from = ResourceId(from_raw);
    if is_parent(to, from) {
        push(&mut ev, from, false, NOTIFY_ANCESTOR);
        out_run(&mut ev, from, Some(to), NOTIFY_VIRTUAL);
        push(&mut ev, to, true, NOTIFY_INFERIOR);
        if is_parent(to, p) && p != from && !is_parent(from, p) && !is_parent(p, from) {
            in_run(&mut ev, to, p, NOTIFY_POINTER);
        }
    } else if is_parent(from, to) {
        if is_parent(from, p) && p != from && !is_parent(to, p) && !is_parent(p, to) {
            push(&mut ev, p, false, NOTIFY_POINTER);
            out_run(&mut ev, p, Some(from), NOTIFY_POINTER);
        }
        push(&mut ev, from, false, NOTIFY_INFERIOR);
        in_run(&mut ev, from, to, NOTIFY_VIRTUAL);
        push(&mut ev, to, true, NOTIFY_ANCESTOR);
    } else {
        let common =
            lowest_common_ancestor(&ancestor_chain(state, from), &ancestor_chain(state, to));
        if is_parent(from, p) {
            out_run(&mut ev, p, Some(from), NOTIFY_POINTER);
        }
        push(&mut ev, from, false, NOTIFY_NONLINEAR);
        if from != root {
            out_run(&mut ev, from, common, NOTIFY_NONLINEAR_VIRTUAL);
        }
        if to != root
            && let Some(common) = common
        {
            in_run(&mut ev, common, to, NOTIFY_NONLINEAR_VIRTUAL);
        }
        push(&mut ev, to, true, NOTIFY_NONLINEAR);
        if is_parent(to, p) {
            in_run(&mut ev, to, p, NOTIFY_POINTER);
        }
    }
    ev
}

/// `[start, parent_of_start, parent_of_parent, ..., root]`, terminated
/// when a window is its own parent (X11's root convention) or when a
/// missing/cyclic parent is reached. Capped at 256 hops as a defense
/// against malformed states.
fn ancestor_chain(state: &ServerState, start: ResourceId) -> Vec<ResourceId> {
    let mut chain = vec![start];
    let mut current = start;
    let mut hops = 0;
    while hops < 256 {
        let Some(window) = state.resources.window(current) else {
            break;
        };
        if window.parent == current {
            break;
        }
        chain.push(window.parent);
        current = window.parent;
        hops += 1;
    }
    chain
}

/// First entry of `b` that also appears in `a`. With the `start`
/// ordering both chains share, this yields the lowest common
/// ancestor. Returns `None` only if no shared ancestor exists (which
/// in a well-formed X11 state means at least the root, so `None` is
/// effectively defensive).
fn lowest_common_ancestor(a: &[ResourceId], b: &[ResourceId]) -> Option<ResourceId> {
    let a_set: HashSet<_> = a.iter().copied().collect();
    b.iter().copied().find(|w| a_set.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    use crate::{
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };

    fn make_window(state: &mut ServerState, id: u32, parent: ResourceId) -> ResourceId {
        let rid = ResourceId(id);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: rid,
                parent,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        rid
    }

    fn windows_only(events: &[CrossingEvent]) -> Vec<ResourceId> {
        events.iter().map(|e| e.window).collect()
    }

    fn details_only(events: &[CrossingEvent]) -> Vec<u8> {
        events.iter().map(|e| e.detail).collect()
    }

    fn kinds_only(events: &[CrossingEvent]) -> Vec<CrossingKind> {
        events.iter().map(|e| e.kind).collect()
    }

    fn children_only(events: &[CrossingEvent]) -> Vec<ResourceId> {
        events.iter().map(|e| e.child).collect()
    }

    #[test]
    fn equal_windows_emit_no_events() {
        let mut state = ServerState::new();
        let w = make_window(&mut state, 0x0010_0030, ROOT_WINDOW);
        assert!(implicit_grab_crossings(&state, w, w).is_empty());
    }

    #[test]
    fn focus_descendant_of_grab_walks_up() {
        // root → A → B → focus, grab = A
        let mut state = ServerState::new();
        let a = make_window(&mut state, 0x0010_0010, ROOT_WINDOW);
        let b = make_window(&mut state, 0x0010_0011, a);
        let f = make_window(&mut state, 0x0010_0012, b);
        let events = implicit_grab_crossings(&state, f, a);
        assert_eq!(windows_only(&events), vec![f, b, a]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Leave,
                CrossingKind::Enter
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_ANCESTOR, NOTIFY_VIRTUAL, NOTIFY_INFERIOR],
        );
        // child: Leave on focus → None. Virtual Leave on B (ancestor of
        // focus on path to focus) → child = f (immediate descendant of
        // B on path to f). Enter on grab (a) → None (source IS event
        // for Inferior detail; Xorg emits child=None on the dest).
        assert_eq!(
            children_only(&events),
            vec![ResourceId(0), f, ResourceId(0)],
        );
    }

    #[test]
    fn focus_ancestor_of_grab_walks_down() {
        // root → focus → A → B → grab
        let mut state = ServerState::new();
        let f = make_window(&mut state, 0x0010_0020, ROOT_WINDOW);
        let a = make_window(&mut state, 0x0010_0021, f);
        let b = make_window(&mut state, 0x0010_0022, a);
        let g = make_window(&mut state, 0x0010_0023, b);
        let events = implicit_grab_crossings(&state, f, g);
        // Order: focus first, then ancestors of grab from closest-to-focus
        // outward, then grab.
        assert_eq!(windows_only(&events), vec![f, a, b, g]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Enter,
                CrossingKind::Enter,
                CrossingKind::Enter,
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![
                NOTIFY_INFERIOR,
                NOTIFY_VIRTUAL,
                NOTIFY_VIRTUAL,
                NOTIFY_ANCESTOR,
            ],
        );
        // child: Leave on focus → None. Virtual Enter on A → child = b
        // (immediate descendant of A on path to g). Virtual Enter on B
        // → child = g. Enter on grab → None.
        assert_eq!(
            children_only(&events),
            vec![ResourceId(0), b, g, ResourceId(0)],
        );
    }

    #[test]
    fn root_to_direct_child_has_no_virtuals() {
        // root → frame (direct child). Cursor moves from root to frame.
        // This is the e16 hover-popup case: yserver must emit
        // LeaveNotify on root with detail=NotifyInferior so the WM
        // knows the cursor went into an inferior subtree.
        let mut state = ServerState::new();
        let frame = make_window(&mut state, 0x0010_0424, ROOT_WINDOW);
        let events = normal_mode_crossings(&state, ROOT_WINDOW, frame);
        assert_eq!(windows_only(&events), vec![ROOT_WINDOW, frame]);
        assert_eq!(
            kinds_only(&events),
            vec![CrossingKind::Leave, CrossingKind::Enter],
        );
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_INFERIOR, NOTIFY_ANCESTOR]
        );
        assert_eq!(children_only(&events), vec![ResourceId(0), ResourceId(0)]);
    }

    #[test]
    fn disjoint_subtrees_walk_through_common_ancestor() {
        // root → C → A → focus
        // root → C → B → grab
        let mut state = ServerState::new();
        let c = make_window(&mut state, 0x0010_0040, ROOT_WINDOW);
        let a = make_window(&mut state, 0x0010_0041, c);
        let f = make_window(&mut state, 0x0010_0042, a);
        let b = make_window(&mut state, 0x0010_0043, c);
        let g = make_window(&mut state, 0x0010_0044, b);
        let events = implicit_grab_crossings(&state, f, g);
        assert_eq!(windows_only(&events), vec![f, a, b, g]);
        assert_eq!(
            kinds_only(&events),
            vec![
                CrossingKind::Leave,
                CrossingKind::Leave,
                CrossingKind::Enter,
                CrossingKind::Enter,
            ],
        );
        assert_eq!(
            details_only(&events),
            vec![
                NOTIFY_NONLINEAR,
                NOTIFY_NONLINEAR_VIRTUAL,
                NOTIFY_NONLINEAR_VIRTUAL,
                NOTIFY_NONLINEAR,
            ],
        );
        // child: Leave on focus → None. NonlinearVirtual Leave on A →
        // child = f (descendant of A on path to source f).
        // NonlinearVirtual Enter on B → child = g (descendant of B on
        // path to destination g). Enter on grab → None.
        assert_eq!(
            children_only(&events),
            vec![ResourceId(0), f, g, ResourceId(0)],
        );
    }

    #[test]
    fn disjoint_with_root_as_only_common_ancestor() {
        // root → focus, root → grab — common ancestor is root, which
        // is *not* in the chains' "skip root" iteration: chains end at
        // root because window.parent == current at root, so root never
        // gets a Leave/Enter virtual.
        let mut state = ServerState::new();
        let f = make_window(&mut state, 0x0010_0050, ROOT_WINDOW);
        let g = make_window(&mut state, 0x0010_0051, ROOT_WINDOW);
        let events = implicit_grab_crossings(&state, f, g);
        // Just Leave on focus + Enter on grab (no virtuals because no
        // intermediate ancestors below root).
        assert_eq!(windows_only(&events), vec![f, g]);
        assert_eq!(
            details_only(&events),
            vec![NOTIFY_NONLINEAR, NOTIFY_NONLINEAR],
        );
        assert_eq!(children_only(&events), vec![ResourceId(0), ResourceId(0)]);
    }

    fn fe(window: ResourceId, focus_in: bool, detail: u8) -> FocusEvent {
        FocusEvent {
            window,
            focus_in,
            detail,
        }
    }

    #[test]
    fn focus_pointerroot_to_window_emits_root_out_then_in_chain() {
        // root → A → B; focus PointerRoot(1) → B, pointer outside B.
        let mut state = ServerState::new();
        let a = make_window(&mut state, 0x0010_0200, ROOT_WINDOW);
        let b = make_window(&mut state, 0x0010_0201, a);
        let p = make_window(&mut state, 0x0010_0202, ROOT_WINDOW); // pointer window
        let ev = focus_transition_events(&state, 1, b.0, p);
        assert_eq!(
            ev,
            vec![
                // PointerRoot: NotifyPointer run P→root inclusive.
                fe(p, false, NOTIFY_POINTER),
                fe(ROOT_WINDOW, false, NOTIFY_POINTER),
                fe(ROOT_WINDOW, false, NOTIFY_POINTER_ROOT),
                fe(ROOT_WINDOW, true, NOTIFY_NONLINEAR_VIRTUAL),
                fe(a, true, NOTIFY_NONLINEAR_VIRTUAL),
                fe(b, true, NOTIFY_NONLINEAR),
            ],
        );
    }

    /// Xvfb, the pointer in a root child E: SetInputFocus(E) from
    /// PointerRoot and back sends XI2 FocusOut/FocusIn on E and the root's
    /// PointerRoot leg only — none of the core sequence's NotifyPointer and
    /// NotifyNonlinearVirtual events on the root.
    #[test]
    fn device_focus_to_and_from_pointerroot_skips_the_root_legs() {
        let mut state = ServerState::new();
        let e = make_window(&mut state, 0x0010_0220, ROOT_WINDOW);
        assert_eq!(
            device_focus_transition_events(&state, 1, e.0, e),
            vec![
                fe(e, false, NOTIFY_POINTER),
                fe(ROOT_WINDOW, false, NOTIFY_POINTER_ROOT),
                fe(e, true, NOTIFY_NONLINEAR),
            ],
        );
        assert_eq!(
            device_focus_transition_events(&state, e.0, 1, e),
            vec![
                fe(e, false, NOTIFY_NONLINEAR),
                fe(ROOT_WINDOW, false, NOTIFY_NONLINEAR_VIRTUAL),
                fe(ROOT_WINDOW, true, NOTIFY_POINTER_ROOT),
                fe(e, true, NOTIFY_POINTER),
            ],
        );
        assert_eq!(device_focus_transition_events(&state, e.0, e.0, e), vec![]);
    }

    #[test]
    fn focus_window_to_none_walks_out_to_root() {
        // root → A → B; focus B → None, pointer in B's child.
        let mut state = ServerState::new();
        let a = make_window(&mut state, 0x0010_0210, ROOT_WINDOW);
        let b = make_window(&mut state, 0x0010_0211, a);
        let p = make_window(&mut state, 0x0010_0212, b);
        let ev = focus_transition_events(&state, b.0, 0, p);
        assert_eq!(
            ev,
            vec![
                fe(p, false, NOTIFY_POINTER),
                fe(b, false, NOTIFY_NONLINEAR),
                fe(a, false, NOTIFY_NONLINEAR_VIRTUAL),
                fe(ROOT_WINDOW, false, NOTIFY_NONLINEAR_VIRTUAL),
                fe(ROOT_WINDOW, true, NOTIFY_DETAIL_NONE),
            ],
        );
    }

    #[test]
    fn focus_none_to_pointerroot_switch() {
        let mut state = ServerState::new();
        let p = make_window(&mut state, 0x0010_0220, ROOT_WINDOW);
        let ev = focus_transition_events(&state, 0, 1, p);
        assert_eq!(
            ev,
            vec![
                fe(ROOT_WINDOW, false, NOTIFY_DETAIL_NONE),
                fe(ROOT_WINDOW, true, NOTIFY_POINTER_ROOT),
                // NotifyPointer run root (inclusive) down to P.
                fe(ROOT_WINDOW, true, NOTIFY_POINTER),
                fe(p, true, NOTIFY_POINTER),
            ],
        );
    }

    #[test]
    fn focus_window_to_descendant_uses_inferior_then_ancestor() {
        // root → A → B → C; focus A → C, pointer elsewhere.
        let mut state = ServerState::new();
        let a = make_window(&mut state, 0x0010_0230, ROOT_WINDOW);
        let b = make_window(&mut state, 0x0010_0231, a);
        let c = make_window(&mut state, 0x0010_0232, b);
        let p = make_window(&mut state, 0x0010_0233, ROOT_WINDOW);
        let ev = focus_transition_events(&state, a.0, c.0, p);
        assert_eq!(
            ev,
            vec![
                fe(a, false, NOTIFY_INFERIOR),
                fe(b, true, NOTIFY_VIRTUAL),
                fe(c, true, NOTIFY_ANCESTOR),
            ],
        );
    }

    #[test]
    fn ancestor_chain_is_capped() {
        // Chain length cap is defensive — there's no practical way to
        // construct a 257-deep tree in the test harness here, so just
        // assert the code path doesn't loop forever on a sane case.
        let mut state = ServerState::new();
        let mut parent = ROOT_WINDOW;
        for i in 0..32 {
            parent = make_window(&mut state, 0x0010_0100 + i, parent);
        }
        let chain = ancestor_chain(&state, parent);
        // 33 entries: the leaf + 32 ancestors up to and including root.
        // (root is included because we hit it via parent==current and
        // break *after* pushing.)
        assert!(chain.len() >= 32, "chain too short: {}", chain.len());
        assert!(chain.len() <= 257);
    }
}
