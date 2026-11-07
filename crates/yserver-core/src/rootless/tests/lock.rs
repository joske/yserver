//! Pointer lock and relative motion (`rootless/lock.rs`).

use super::RecordingHost;
use crate::{
    backend::recording::RecordedCall,
    core_loop::{HostInputEvent, InputOrigin, handle_host_input},
    resources::ROOT_WINDOW,
    rootless::{HostRequest, RootlessEvent, RootlessInput, ToplevelId},
};

const GAME: u32 = 0x0020_0001;
const ORIGIN: InputOrigin = InputOrigin::NestedHost;

fn input(host: &mut RecordingHost, input: RootlessInput) {
    handle_host_input(
        &mut host.state,
        &mut host.backend,
        HostInputEvent::Rootless(input),
    );
}

fn lock_events(events: &[RootlessEvent]) -> Vec<Option<ToplevelId>> {
    events
        .iter()
        .filter_map(|e| match e {
            RootlessEvent::PointerLock { id } => Some(*id),
            _ => None,
        })
        .collect()
}

/// A game: mapped at (100, 50), 200x100, the host's pointer and keyboard
/// on it, the cursor hidden.
fn game() -> (RecordingHost, ToplevelId) {
    let mut host = RecordingHost::new();
    host.create_window(1, GAME, ROOT_WINDOW.0, (100, 50), (200, 100), 0, false);
    let id = host.map_and_reconcile(1, GAME);
    input(
        &mut host,
        RootlessInput::PointerEnter {
            origin: ORIGIN,
            toplevel: id,
            local: (10.0, 10.0),
            time: 1,
        },
    );
    host.host(HostRequest::Activated { id });
    host.backend.displayed_cursor_hidden = true;
    (host, id)
}

fn last_motion(host: &RecordingHost) -> Option<(i32, i32)> {
    host.backend.calls().iter().rev().find_map(|c| match c {
        RecordedCall::HostPointer {
            x, y, button: 0, ..
        } => Some((*x, *y)),
        _ => None,
    })
}

/// Xwayland's rule: a warp into the focused top-level under the pointer,
/// with the cursor hidden, locks; the next reconcile says nothing more.
#[test]
fn a_warp_into_the_focused_window_with_a_hidden_cursor_locks() {
    let (mut host, id) = game();
    assert!(lock_events(&host.reconcile()).is_empty(), "no warp yet");
    // The recording backend moves no pointer: as a real one would.
    host.state.pointer_root = (200, 100);
    crate::rootless::note_warp(&mut host.state, 200, 100);
    assert_eq!(lock_events(&host.reconcile()), [Some(id)]);
    assert!(lock_events(&host.reconcile()).is_empty(), "held, unchanged");
}

/// A shown cursor never locks; showing it lets a lock go.
#[test]
fn a_shown_cursor_lets_the_lock_go() {
    let (mut host, id) = game();
    host.backend.displayed_cursor_hidden = false;
    crate::rootless::note_warp(&mut host.state, 200, 100);
    assert!(lock_events(&host.reconcile()).is_empty());
    host.backend.displayed_cursor_hidden = true;
    crate::rootless::note_warp(&mut host.state, 200, 100);
    assert_eq!(lock_events(&host.reconcile()), [Some(id)]);
    host.backend.displayed_cursor_hidden = false;
    assert_eq!(lock_events(&host.reconcile()), [None]);
}

/// While locked, relative motion moves X's pointer by its whole pixels,
/// the fraction kept for the next; without a lock it is dropped.
#[test]
fn relative_motion_moves_the_pointer_only_while_locked() {
    let (mut host, _) = game();
    let at = last_motion(&host).expect("the enter moved the pointer");
    let relative = |dx: f64| RootlessInput::PointerRelative {
        origin: ORIGIN,
        delta: (dx, 0.0),
        unaccelerated: (dx, 0.0),
        time: 7,
    };
    input(&mut host, relative(5.0));
    assert_eq!(last_motion(&host), Some(at), "no lock: dropped");
    host.state.pointer_root = (200, 100);
    crate::rootless::note_warp(&mut host.state, 200, 100);
    let _ = host.reconcile();
    input(&mut host, relative(2.6));
    assert_eq!(last_motion(&host), Some((202, 100)));
    // The recording backend moves no pointer: as a real one would.
    host.state.pointer_root = (202, 100);
    input(&mut host, relative(0.6));
    assert_eq!(
        last_motion(&host),
        Some((203, 100)),
        "0.6 + 0.6 carries one"
    );
}
