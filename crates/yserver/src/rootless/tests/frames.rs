//! The pure presenter model against the spec's rules (§4.5, §4.5.1, §4.5.2).

use yserver_core::rootless::Rect;

use crate::rootless::{
    frames::{Delivery, FillPlan, POOL, Readiness, WindowFrames},
    host::FrameSeq,
};

const GEN: u64 = 7;
const FULL: Rect = rect(0, 0, 44, 34);
const A: Rect = rect(2, 3, 4, 5);
const B: Rect = rect(10, 11, 6, 7);

const fn rect(x: i32, y: i32, width: u16, height: u16) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Past setup, frame 1 (full) delivered, `Ready` again.
fn shown() -> WindowFrames {
    let mut f = WindowFrames::new(GEN);
    f.on_setup_done();
    let seq = f.submitted(vec![FULL], None);
    f.on_delivered(seq);
    f.on_ready();
    f
}

/// §4.5.1 diagram.
#[test]
fn setup_then_first_frame_then_ready() {
    let mut f = WindowFrames::new(GEN);
    assert_eq!(f.readiness(), Readiness::Setup);
    assert!(!f.can_submit(), "no frame before SetupDone");
    f.on_setup_done();
    assert_eq!(f.readiness(), Readiness::ReadyFirst);
    assert!(f.can_submit());
    let seq = f.submitted(vec![FULL], None);
    assert_eq!(seq, FrameSeq(1));
    assert_eq!(f.readiness(), Readiness::Busy);
    assert!(f.on_delivered(seq).first);
    f.on_ready();
    assert_eq!(f.readiness(), Readiness::Ready);
}

/// §4.5.1: `Delivered`/`Dropped` do not by themselves make the window ready.
#[test]
fn ready_is_not_implied_by_delivered() {
    let mut f = WindowFrames::new(GEN);
    f.on_setup_done();
    let seq = f.submitted(vec![FULL], None);
    f.on_delivered(seq);
    assert!(!f.can_submit());
    f.on_ready();
    let seq = f.submitted(vec![A], None);
    assert!(
        !f.on_delivered(seq).first,
        "only the first delivery is first"
    );
    assert!(!f.can_submit());
}

/// §4.5.1 liveness (a): a dropped first frame goes back to `ReadyFirst`,
/// stays invisible, and the retry is full-extent.
#[test]
fn dropped_first_frame_returns_to_ready_first() {
    let mut f = WindowFrames::new(GEN);
    f.on_setup_done();
    let one = f.submitted(vec![FULL], None);
    f.on_dropped(one);
    assert_eq!(f.readiness(), Readiness::Busy, "Dropped alone is not Ready");
    f.on_ready();
    assert_eq!(f.readiness(), Readiness::ReadyFirst);
    let two = f.submitted(vec![A], None);
    assert_eq!(f.frame_damage(two, FULL), [FULL]);
    assert!(
        f.on_delivered(two).first,
        "invisible until the retry is delivered"
    );
}

/// §4.5.2: damage since the last DELIVERED frame, dropped frames included.
#[test]
fn damage_since_last_delivered_includes_dropped_frames() {
    let mut f = shown();
    let two = f.submitted(vec![A], None);
    f.on_dropped(two);
    f.on_ready();
    let three = f.submitted(vec![B], None);
    assert_eq!(f.frame_damage(three, FULL), [A, B]);
}

/// §4.5.2: a history that does not reach back to the last delivered frame
/// gives the full extent.
#[test]
fn history_shorter_than_gap_gives_full() {
    let mut f = shown();
    for _ in 2..=5 {
        let seq = f.submitted(vec![A], None);
        f.on_dropped(seq);
        f.on_ready();
    }
    let six = f.submitted(vec![B], None);
    assert_eq!(six, FrameSeq(6));
    assert_eq!(POOL, 3);
    assert_eq!(f.frame_damage(six, FULL), [FULL]);
}

/// §4.5.2: a buffer is filled from its own seq, independently of delivery.
#[test]
fn fill_plan_uses_the_buffers_own_seq() {
    let mut f = WindowFrames::new(GEN);
    f.on_setup_done();
    let damage = [
        FULL,
        A,
        B,
        rect(1, 1, 1, 1),
        rect(20, 0, 2, 2),
        rect(0, 20, 3, 3),
    ];
    for d in damage {
        let seq = f.submitted(vec![d], None);
        f.on_delivered(seq);
        f.on_ready();
    }
    let six = FrameSeq(6);
    assert_eq!(
        f.fill_plan(Some((FrameSeq(4), GEN)), six),
        FillPlan::Rects(vec![damage[4], damage[5]])
    );
    assert_eq!(f.fill_plan(Some((FrameSeq(1), GEN)), six), FillPlan::Full);
    assert_eq!(
        f.fill_plan(Some((FrameSeq(5), GEN + 1)), six),
        FillPlan::Full
    );
    assert_eq!(f.fill_plan(None, six), FillPlan::Full);
}

/// §4.5 resize rule: a new backing generation starts from a full frame.
#[test]
fn new_generation_forgets_delivery() {
    let mut f = shown();
    f.new_generation(GEN + 1);
    assert_eq!(f.backing_gen(), GEN + 1);
    let seq = f.submitted(vec![A], None);
    assert_eq!(f.frame_damage(seq, FULL), [FULL]);
    assert_eq!(
        f.fill_plan(Some((FrameSeq(1), GEN + 1)), seq),
        FillPlan::Full
    );
}

/// §4.5.2 "Order": `Delivered(n)` acks every held snapshot up to `n`, with
/// the newest one's epoch.
#[test]
fn delivered_acks_the_newest_held_snapshot() {
    let held = || {
        let mut f = WindowFrames::new(GEN);
        f.on_setup_done();
        f.submitted(vec![FULL], Some(10));
        f.on_ready();
        f.submitted(vec![A], Some(14));
        f
    };
    let mut f = held();
    assert_eq!(
        f.on_delivered(FrameSeq(2)),
        Delivery {
            first: true,
            ack_epoch: Some(14)
        }
    );
    assert_eq!(f.on_delivered(FrameSeq(2)).ack_epoch, None, "held is empty");

    let mut f = held();
    assert_eq!(f.on_delivered(FrameSeq(1)).ack_epoch, Some(10));
    assert_eq!(
        f.on_delivered(FrameSeq(2)).ack_epoch,
        Some(14),
        "(2, e14) kept"
    );
}

/// §4.5: a resize's held snapshot (of the old pixmap) is never acked.
#[test]
fn new_generation_drops_held_snapshots() {
    let mut f = WindowFrames::new(GEN);
    f.on_setup_done();
    let one = f.submitted(vec![FULL], Some(10));
    f.new_generation(GEN + 1);
    assert_eq!(f.on_delivered(one).ack_epoch, None);
}

/// §4.5.1 visibility: only a frame of the current generation shows the
/// window; a late `Delivered` of a pre-resize first frame does not.
#[test]
fn late_delivered_of_an_old_generation_does_not_show() {
    let mut f = WindowFrames::new(GEN);
    f.on_setup_done();
    let old = f.submitted(vec![FULL], None);
    f.new_generation(GEN + 1);
    assert!(!f.on_delivered(old).first);
    f.on_ready();
    assert_eq!(f.readiness(), Readiness::ReadyFirst, "still invisible");
    let new = f.submitted(vec![FULL], None);
    assert!(f.on_delivered(new).first);
}

/// A native window made again shows nothing and takes no frame until its
/// setup is done, then a whole one: no frame before it, delivered late or
/// in a buffer, stands for it.
#[test]
fn a_window_made_again_starts_over_at_its_setup() {
    let mut f = shown();
    let late = f.submitted(vec![A], None);
    f.restart();
    assert_eq!(f.readiness(), Readiness::Setup);
    assert!(!f.can_submit(), "no frame before the new setup is done");
    assert!(!f.on_delivered(late).first, "a frame for the window before");
    f.on_setup_done();
    assert!(f.can_submit());
    let first = f.submitted(vec![B], None);
    assert_eq!(f.frame_damage(first, FULL), [FULL]);
    assert_eq!(f.fill_plan(Some((late, GEN)), first), FillPlan::Full);
    assert!(f.on_delivered(first).first, "shown again");
}
