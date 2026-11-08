//! Pointer lock: a game's grab of a hidden pointer becomes the host's
//! locked pointer, and the host's relative motion moves X's pointer.
//!
//! A game that turns the mouse into a direction grabs the pointer to its
//! window, hides the cursor and warps the pointer back to the window's
//! centre after each motion, reading how far it moved (core `MotionNotify`
//! from the centre, or `XInput2` raw motion). On a host whose window system
//! moves the pointer itself that pattern breaks: X's warp moves X's pointer
//! only, the host's runs into the screen's edge, and the next absolute
//! motion jumps X's pointer to wherever the host's is. So the core asks the
//! host to lock the pointer on the game's top-level
//! ([`RootlessEvent::PointerLock`](super::RootlessEvent::PointerLock)), and
//! while the lock is in force each
//! [`RootlessInput::PointerRelative`](super::RootlessInput::PointerRelative)
//! moves X's pointer by its distance from where X has it, which after a warp
//! is the centre.
//!
//! When to lock starts from Xwayland's rule (`hw/xwayland/xwayland-input.c`,
//! `xwl_seat_maybe_lock_on_hidden_cursor` and
//! `xwl_seat_emulate_pointer_warp`), for a server whose windows a host
//! manages: the cursor is hidden (the `None` cursor or XFIXES `HideCursor`),
//! the host's pointer is on the top-level (the pointer scope), and a grab
//! confines the pointer to it or a `WarpPointer` lands inside it. The lock
//! is let go when the cursor is shown again, the host's pointer leaves the
//! top-level, the confining grab ends, and, for a lock taken by a warp,
//! when X's pointer moves out of the top-level
//! (`xwl_pointer_warp_emulator_handle_motion`).
//!
//! Three parts of the rule are ours, not Xwayland's:
//!
//! * a cursor whose every pixel is transparent counts as hidden, which is
//!   how SDL and most games hide it (`Backend::displayed_cursor_hidden`);
//! * the host's keyboard must be on the top-level too
//!   (`HostRequest::Activated`), and the lock is let go when it leaves;
//! * a lock taken by a warp while a grab is active is let go when the grab
//!   ends, as a confined one is; Xwayland keeps a warp's lock until the
//!   cursor is shown or X's pointer moves out.

use yserver_protocol::x11::ResourceId;

use super::{Rootless, RootlessEvent, seat::PointerScope};
use crate::{resources::ROOT_WINDOW, server::ServerState};

/// What the server and the host say now, as far as the lock goes.
/// Windows are X top-levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scene {
    /// The cursor in effect is hidden.
    pub hidden: bool,
    /// The top-level the host's pointer is on.
    pub pointer_on: Option<ResourceId>,
    /// The top-level the host's keyboard is on (`HostRequest::Activated`).
    pub keyboard_on: Option<ResourceId>,
    /// An active pointer grab a client asked for (not the implicit one of a
    /// button held).
    pub grabbed: bool,
    /// The top-level the grab confines the pointer to.
    pub confined_to: Option<ResourceId>,
    /// The top-level a `WarpPointer` since the last look landed inside.
    pub warped_into: Option<ResourceId>,
    /// X's pointer is inside the top-level the lock is on.
    pub inside_held: bool,
}

/// A lock the server holds: on which top-level, and whether it was taken
/// under a grab, whose end lets it go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub window: ResourceId,
    pub grabbed: bool,
}

/// What to do about the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Nothing changes.
    Stay,
    /// Lock the pointer on this top-level.
    Lock(Held),
    /// Let the lock go, for this reason.
    Release(&'static str),
}

/// What to do, given the scene and the lock held.
pub(crate) fn decide(scene: &Scene, held: Option<Held>) -> Verdict {
    if let Some(held) = held {
        let release = if !scene.hidden {
            Some("the cursor is shown")
        } else if scene.pointer_on != Some(held.window) {
            Some("the pointer left the window")
        } else if scene.keyboard_on != Some(held.window) {
            Some("the window lost the keyboard")
        } else if held.grabbed && !scene.grabbed {
            Some("the grab ended")
        } else if !held.grabbed && !scene.grabbed && !scene.inside_held {
            Some("the X pointer left the window")
        } else {
            None
        };
        return release.map_or(Verdict::Stay, Verdict::Release);
    }
    let Some(window) = scene.pointer_on else {
        return Verdict::Stay;
    };
    if !scene.hidden || scene.keyboard_on != Some(window) {
        return Verdict::Stay;
    }
    if scene.confined_to == Some(window) || scene.warped_into == Some(window) {
        return Verdict::Lock(Held {
            window,
            grabbed: scene.grabbed,
        });
    }
    Verdict::Stay
}

/// A distance in pixels, with what is left of the last one, as whole
/// pixels and the new remainder.
pub(crate) fn whole(residue: &mut f64, distance: f64) -> i32 {
    let sum = *residue + distance;
    let whole = sum.trunc();
    *residue = sum - whole;
    #[allow(clippy::cast_possible_truncation)]
    let whole = whole as i32;
    whole
}

/// The lock's share of [`Rootless`].
#[derive(Debug, Default)]
pub(crate) struct LockState {
    /// The lock the host was asked to hold.
    held: Option<Held>,
    /// Where the last `WarpPointer` since the lock was last looked at put
    /// the pointer, in root coordinates.
    warped: Option<(i32, i32)>,
    /// What is left of a locked motion's distance after its whole pixels:
    /// the pointer's and the raw motion's, across and down.
    pub(crate) residue: [f64; 4],
}

impl LockState {
    pub(crate) fn held(&self) -> bool {
        self.held.is_some()
    }
}

/// A client's `WarpPointer` put X's pointer at root `(x, y)`: a game's warp
/// to its window's centre, which may take the lock. No-op when rootless is
/// off.
pub(crate) fn note_warp(state: &mut ServerState, x: i32, y: i32) {
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.lock.warped = Some((x, y));
    }
}

/// The top-level `window` is, or is inside.
fn toplevel_of(state: &ServerState, window: ResourceId) -> Option<ResourceId> {
    let mut current = window;
    for _ in 0..256 {
        match state.resources.parent_of(current) {
            Some(parent) if parent == ROOT_WINDOW => return Some(current),
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
    None
}

/// Whether root position `(x, y)` is inside `window`'s content.
fn inside(state: &ServerState, window: ResourceId, (x, y): (i32, i32)) -> bool {
    let Some(found) = state.resources.window(window) else {
        return false;
    };
    let (left, top) = state.resources.window_absolute_position(window);
    (left..left + i32::from(found.width)).contains(&x)
        && (top..top + i32::from(found.height)).contains(&y)
}

/// Look at the lock once per reconcile: the event that takes or lets go of
/// it, if the verdict changed. `hidden` is the displayed cursor's.
pub(super) fn reconcile(
    rootless: &mut Rootless,
    state: &ServerState,
    hidden: bool,
) -> Option<RootlessEvent> {
    let warped = rootless.lock.warped.take();
    let tracked = |window: ResourceId| rootless.toplevels.contains_key(&window);
    let pointer_on = match rootless.scope {
        PointerScope::Within(window) if tracked(window) => Some(window),
        _ => None,
    };
    let keyboard_on = rootless.active.filter(|window| tracked(*window));
    let grab = state
        .active_pointer_grab
        .as_ref()
        .filter(|grab| !grab.implicit);
    let confined_to = (grab.is_some() && state.pointer_confine_to.0 != 0)
        .then(|| toplevel_of(state, state.pointer_confine_to))
        .flatten();
    let held = rootless.lock.held;
    let at = (
        i32::from(state.pointer_root.0),
        i32::from(state.pointer_root.1),
    );
    let scene = Scene {
        hidden,
        pointer_on,
        keyboard_on,
        grabbed: grab.is_some(),
        confined_to,
        warped_into: warped
            .zip(pointer_on)
            .filter(|&(at, window)| inside(state, window, at))
            .map(|(_, window)| window),
        inside_held: held.is_some_and(|held| inside(state, held.window, at)),
    };
    let id = match decide(&scene, held) {
        Verdict::Stay => return None,
        Verdict::Lock(new) => {
            log::info!(
                "rootless: a client holds the pointer; locking it on 0x{:x}{}",
                new.window.0,
                if new.grabbed { " (grabbed)" } else { "" }
            );
            rootless.lock.held = Some(new);
            Some(rootless.toplevels.get(&new.window)?.id)
        }
        Verdict::Release(why) => {
            log::info!("rootless: the pointer lock is let go: {why}");
            rootless.lock.held = None;
            None
        }
    };
    rootless.lock.residue = [0.0; 4];
    Some(RootlessEvent::PointerLock { id })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAME: ResourceId = ResourceId(0x20_0001);
    const OTHER: ResourceId = ResourceId(0x40_0001);

    /// The game's scene: cursor hidden, pointer and keyboard on it, a grab
    /// confining the pointer to it.
    fn game() -> Scene {
        Scene {
            hidden: true,
            pointer_on: Some(GAME),
            keyboard_on: Some(GAME),
            grabbed: true,
            confined_to: Some(GAME),
            warped_into: None,
            inside_held: true,
        }
    }

    #[test]
    fn a_hidden_cursor_confined_to_the_window_locks() {
        assert_eq!(
            decide(&game(), None),
            Verdict::Lock(Held {
                window: GAME,
                grabbed: true
            })
        );
    }

    #[test]
    fn a_warp_into_the_window_with_the_cursor_hidden_locks_without_a_grab() {
        let scene = Scene {
            grabbed: false,
            confined_to: None,
            warped_into: Some(GAME),
            ..game()
        };
        assert_eq!(
            decide(&scene, None),
            Verdict::Lock(Held {
                window: GAME,
                grabbed: false
            })
        );
    }

    #[test]
    fn nothing_locks_with_the_cursor_shown_or_the_pointer_or_keyboard_elsewhere() {
        for scene in [
            Scene {
                hidden: false,
                ..game()
            },
            Scene {
                pointer_on: Some(OTHER),
                ..game()
            },
            Scene {
                pointer_on: None,
                ..game()
            },
            Scene {
                keyboard_on: None,
                ..game()
            },
            Scene {
                confined_to: Some(OTHER),
                ..game()
            },
            Scene {
                confined_to: None,
                warped_into: Some(OTHER),
                ..game()
            },
            Scene {
                confined_to: None,
                ..game()
            },
        ] {
            assert_eq!(decide(&scene, None), Verdict::Stay, "{scene:?}");
        }
    }

    #[test]
    fn a_lock_stays_while_the_game_holds_it() {
        let held = Held {
            window: GAME,
            grabbed: true,
        };
        assert_eq!(decide(&game(), Some(held)), Verdict::Stay);
        // Its warps and the confine come and go; the lock stays.
        let scene = Scene {
            confined_to: None,
            warped_into: Some(GAME),
            ..game()
        };
        assert_eq!(decide(&scene, Some(held)), Verdict::Stay);
    }

    #[test]
    fn a_lock_goes_on_ungrab_focus_loss_or_a_shown_cursor() {
        let held = Held {
            window: GAME,
            grabbed: true,
        };
        let release = |scene: Scene| match decide(&scene, Some(held)) {
            Verdict::Release(why) => why,
            other => panic!("{other:?} for {scene:?}"),
        };
        assert_eq!(
            release(Scene {
                grabbed: false,
                confined_to: None,
                ..game()
            }),
            "the grab ended"
        );
        assert_eq!(
            release(Scene {
                keyboard_on: Some(OTHER),
                ..game()
            }),
            "the window lost the keyboard"
        );
        assert_eq!(
            release(Scene {
                pointer_on: None,
                ..game()
            }),
            "the pointer left the window"
        );
        assert_eq!(
            release(Scene {
                hidden: false,
                ..game()
            }),
            "the cursor is shown"
        );
    }

    #[test]
    fn a_lock_taken_by_a_warp_goes_when_x_moves_the_pointer_out() {
        let held = Held {
            window: GAME,
            grabbed: false,
        };
        let scene = Scene {
            grabbed: false,
            confined_to: None,
            ..game()
        };
        assert_eq!(decide(&scene, Some(held)), Verdict::Stay);
        let out = Scene {
            inside_held: false,
            ..scene
        };
        assert_eq!(
            decide(&out, Some(held)),
            Verdict::Release("the X pointer left the window")
        );
    }

    #[test]
    fn distances_keep_their_fractions_for_the_next() {
        let mut residue = 0.0;
        assert_eq!(whole(&mut residue, 0.6), 0);
        assert_eq!(whole(&mut residue, 0.6), 1);
        assert!((residue - 0.2).abs() < 1e-9);
        assert_eq!(whole(&mut residue, -2.5), -2);
        assert!((residue + 0.3).abs() < 1e-9);
        assert_eq!(whole(&mut residue, 12.0), 11);
    }
}
