//! Per-scanout-BO damage tracking: what each buffer object is missing.
//!
//! Step 3 of
//! `docs/superpowers/plans/2026-09-01-damage-derived-scene-repaint-plan.md`.
//!
//! # The invariant
//!
//! **`missing[bo]` is the set of pixels of `bo` that do not reflect the current
//! scene.** Every operation here is checkable against that one sentence, and
//! nothing else in this module needs to be held in the head at once.
//!
//! A recycled scanout BO is older than the last frame: it was last painted two
//! or three frames ago, so everything damaged since is stale in it. Without
//! this bookkeeping a partial repaint into a recycled BO leaves whatever the
//! frames in between changed, which is the shape of the corruption that got
//! earlier clipped-repaint attempts reverted.
//!
//! # Why a separate state machine
//!
//! It is pure: no Vulkan, no KMS, no borrow of the compositor. So the
//! transactional contract can be proven by unit tests directly, with partial
//! regions and every failure path, which is the only way to prove it — under
//! always-Full rendering `painted` is the whole output every frame, so an
//! integration test empties `missing` trivially and proves nothing. (Same
//! vacuity as the damage audit's `full` comparisons.)
//!
//! # Transactions
//!
//! yserver commits BO state at page-flip retirement, not at submit, and both
//! submit-failure paths deliberately fold repaint forward without advancing
//! anything. So this cannot be wlroots' damage ring, which rotates
//! destructively at acquire: a ring that clears at acquire lies whenever the
//! later KMS commit fails — the damage is gone and was never presented.
//!
//! Instead:
//!
//! - [`ScanoutDamage::repaint_for`] is **pure**. Acquiring a BO mutates nothing.
//! - [`ScanoutDamage::commit_submitted`] stages, and is called only once the
//!   submit has actually succeeded. An attempt that fails never staged, so
//!   there is nothing to roll back — `pending` was never touched.
//! - [`ScanoutDamage::retire_success`] applies, [`ScanoutDamage::retire_failure`]
//!   restores. The latter exists for the failure paths that land *after* a
//!   successful submit: a copied-scanout completion failure, or a teardown that
//!   discards an already-staged frame.
//!
//! # The safe fallback
//!
//! [`ScanoutDamage::invalidate`] marks everything stale. It costs one full
//! repaint and can never show a stale pixel, so every lifecycle path that
//! cannot reason precisely about BO identity or content calls it rather than
//! attempting to be clever.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "telemetry accessors land ahead of step 4, which reports them"
    )
)]

use ash::vk;

use super::region::Region;

/// A frame that has been submitted and is awaiting page-flip retirement.
///
/// One per output at most: [`ScanoutDamage::commit_submitted`] is only reached
/// with the tick's flip-pending gate satisfied, which requires the previous
/// frame to have retired.
#[derive(Clone, Debug)]
struct InFlight {
    bo_idx: usize,
    /// The damage this frame took responsibility for — `pending` as it stood at
    /// submit. On retirement it becomes stale in every *other* BO.
    submitted_pending: Region,
    /// What the recorder actually covered, which is not always what was asked
    /// for: the bounding box under bbox rendering, the whole output under a
    /// forced-Full frame, the union of rects under per-rect rendering. This is
    /// what retirement subtracts, and passing anything wider than what was
    /// really drawn is the one way this model leaves stale pixels.
    painted: Region,
}

/// Per-output, per-BO damage state. See the module docs for the invariant.
#[derive(Clone, Debug)]
pub(crate) struct ScanoutDamage {
    /// Damage accrued and not yet attributed to any BO.
    pending: Region,
    /// Per BO: the pixels that do not reflect the current scene.
    missing: Vec<Region>,
    in_flight: Option<InFlight>,
    extent: vk::Extent2D,
}

impl ScanoutDamage {
    /// Fresh state for an output with `bo_count` scanout buffers.
    ///
    /// Every BO starts **fully** missing: a brand-new buffer holds nothing, and
    /// claiming otherwise would let the first frame clip against content that
    /// was never painted. `pending` starts empty because `missing` already
    /// forces a full repaint of whichever BO is acquired first.
    pub(crate) fn new(bo_count: usize, extent: vk::Extent2D) -> Self {
        Self {
            pending: Region::new(),
            missing: vec![Self::full(extent); bo_count],
            in_flight: None,
            extent,
        }
    }

    fn full(extent: vk::Extent2D) -> Region {
        Region::from_rect(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        })
    }

    /// Whole-output damage everywhere, nothing staged: the safe fallback.
    ///
    /// Called by any lifecycle path that invalidates BO contents or loses track
    /// of what is in them — a failed flip, a suspend, a modeset, a teardown that
    /// discards a staged frame, a return from direct scanout. Costs one full
    /// repaint; can never show a stale pixel.
    pub(crate) fn invalidate(&mut self) {
        let full = Self::full(self.extent);
        for m in &mut self.missing {
            m.clone_from(&full);
        }
        self.pending = full;
        self.in_flight = None;
    }

    /// Re-shape for a new BO count or output extent, and invalidate.
    ///
    /// The per-BO vector is indexed by `bo_idx` against the platform's pool, so
    /// it must be rebuilt in lockstep with a pool that changed length — an entry
    /// left over from the old pool would be attributed to a different buffer.
    pub(crate) fn resize(&mut self, bo_count: usize, extent: vk::Extent2D) {
        self.extent = extent;
        self.missing = vec![Self::full(extent); bo_count];
        self.pending = Self::full(extent);
        self.in_flight = None;
    }

    /// Accrue new damage. Idempotent under repeated identical damage, since
    /// [`Region::union_with`] is a set union.
    pub(crate) fn add_damage(&mut self, damage: &Region) {
        self.pending.union_with(damage);
    }

    pub(crate) fn add_rect(&mut self, rect: vk::Rect2D) {
        self.pending.add_rect(rect);
    }

    /// What must be repainted to make `bo_idx` reflect the current scene.
    ///
    /// **Pure** — acquiring a BO mutates nothing, so a tick that later skips or
    /// fails leaves the state exactly as it found it and the next tick
    /// recomputes the same answer.
    ///
    /// `loadable` is false when the BO has never been presented or its contents
    /// were invalidated: there is nothing to preserve, so everything is missing.
    /// The caller must also render Full in that case — `loadOp = LOAD` from a BO
    /// that has never been through a present is not merely stale, it is invalid.
    pub(crate) fn repaint_for(&self, bo_idx: usize, loadable: bool) -> Region {
        if !loadable || bo_idx >= self.missing.len() {
            return Self::full(self.extent);
        }
        let mut repaint = self.pending.clone();
        repaint.union_with(&self.missing[bo_idx]);
        // A frame staged against a *different* BO owns damage that this one has
        // not been told about yet: it left `pending` at submit and only lands in
        // `missing` at retirement. The tick's flip-pending gate means this
        // cannot currently be reached, and it is accounted for anyway so the
        // model stays correct on its own terms rather than on a caller's.
        if let Some(f) = &self.in_flight
            && f.bo_idx != bo_idx
        {
            repaint.union_with(&f.submitted_pending);
        }
        repaint
    }

    /// Stage the frame just submitted for `bo_idx`.
    ///
    /// Call this **after** the submit and atomic commit have succeeded. A failed
    /// attempt must not call it: `pending` is then untouched, so there is nothing
    /// to restore and the next tick recomputes an identical repaint.
    ///
    /// `repaint` is what [`Self::repaint_for`] returned; `painted` is what the
    /// recorder actually covered. `painted` must be a superset — painting less
    /// than was asked for while recording it as painted is what bakes a stale
    /// hole into a BO permanently.
    pub(crate) fn commit_submitted(&mut self, bo_idx: usize, repaint: &Region, painted: &Region) {
        debug_assert!(
            self.in_flight.is_none(),
            "two frames staged at once for one output; the flip-pending gate should \
             have made this impossible"
        );
        debug_assert!(
            painted.contains(repaint),
            "painted region does not cover the repaint region: {} px painted vs {} px \
             asked for. Recording a frame as having painted more than it drew clears \
             `missing` for pixels that were never touched.",
            painted.area(),
            repaint.area(),
        );
        self.in_flight = Some(InFlight {
            bo_idx,
            submitted_pending: std::mem::take(&mut self.pending),
            painted: painted.clone(),
        });
    }

    /// The staged frame reached the screen.
    ///
    /// Its damage is now stale in every *other* BO, and the region it painted is
    /// no longer missing from the one it painted into.
    pub(crate) fn retire_success(&mut self) {
        let Some(f) = self.in_flight.take() else {
            return;
        };
        for (idx, m) in self.missing.iter_mut().enumerate() {
            if idx != f.bo_idx {
                m.union_with(&f.submitted_pending);
            }
        }
        if let Some(m) = self.missing.get_mut(f.bo_idx) {
            m.subtract(&f.painted);
            debug_assert!(
                !m.intersects(&f.painted),
                "pixels remain missing from a BO that just painted them"
            );
        }
    }

    /// The staged frame never reached the screen.
    ///
    /// Its damage returns to `pending` and no BO's state moves. Reached by the
    /// failure paths that land after a successful submit — a copied-scanout
    /// completion failure, or a teardown discarding a staged frame.
    pub(crate) fn retire_failure(&mut self) {
        if let Some(f) = self.in_flight.take() {
            self.pending.union_with(&f.submitted_pending);
        }
    }

    /// True if unattributed damage is waiting to be painted — after
    /// [`Self::invalidate`], after [`Self::retire_failure`], or when a tick added
    /// damage and then could not submit. The tick's empty-damage skip and the
    /// backend's compose-wanted predicate both consult this; neither can see it
    /// through the producers, so without it an invalidated output stays stale
    /// until something unrelated damages it.
    pub(crate) fn owes_repaint(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn has_staged_frame(&self) -> bool {
        self.in_flight.is_some()
    }

    pub(crate) fn bo_count(&self) -> usize {
        self.missing.len()
    }

    /// Unattributed damage area, for telemetry.
    pub(crate) fn pending_area(&self) -> u64 {
        self.pending.area()
    }

    /// Stale area of one BO, for telemetry and for the gate that asks whether a
    /// run actually exercised the per-BO model rather than only `pending`.
    pub(crate) fn missing_area(&self, bo_idx: usize) -> u64 {
        self.missing.get(bo_idx).map_or(0, Region::area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `invalidate` and a failed retirement leave work owed that no producer
    /// reports; a fresh model and a committed frame owe nothing.
    #[test]
    fn owes_repaint_follows_pending() {
        let extent = vk::Extent2D {
            width: 100,
            height: 50,
        };
        let full = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        let mut d = ScanoutDamage::new(2, extent);
        assert!(
            !d.owes_repaint(),
            "fresh: missing is full but nothing is pending"
        );
        d.invalidate();
        assert!(d.owes_repaint(), "invalidated: the whole output is owed");
        let repaint = d.repaint_for(0, false);
        d.commit_submitted(0, &repaint, &Region::from_rect(full));
        assert!(!d.owes_repaint(), "staged: pending moved in flight");
        d.retire_failure();
        assert!(d.owes_repaint(), "never reached the screen: owed again");
        let repaint = d.repaint_for(0, false);
        d.commit_submitted(0, &repaint, &Region::from_rect(full));
        d.retire_success();
        assert!(!d.owes_repaint());
    }
    use std::collections::BTreeSet;

    const W: u32 = 16;
    const H: u32 = 16;

    fn extent() -> vk::Extent2D {
        vk::Extent2D {
            width: W,
            height: H,
        }
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    fn reg(rects: &[vk::Rect2D]) -> Region {
        let mut r = Region::new();
        for &rc in rects {
            r.add_rect(rc);
        }
        r
    }

    fn full_region() -> Region {
        reg(&[rect(0, 0, W, H)])
    }

    fn pixels(r: &Region) -> BTreeSet<(i32, i32)> {
        let mut s = BTreeSet::new();
        for rc in r.rects() {
            for y in rc.offset.y..rc.offset.y + rc.extent.height as i32 {
                for x in rc.offset.x..rc.offset.x + rc.extent.width as i32 {
                    s.insert((x, y));
                }
            }
        }
        s
    }

    /// One frame, all the way through: acquire, submit, retire.
    fn frame(d: &mut ScanoutDamage, bo: usize, loadable: bool, painted: Option<&Region>) -> Region {
        let repaint = d.repaint_for(bo, loadable);
        let painted = painted.cloned().unwrap_or_else(|| repaint.clone());
        d.commit_submitted(bo, &repaint, &painted);
        d.retire_success();
        repaint
    }

    // ── the invariant, directly ──────────────────────────────────────

    #[test]
    fn fresh_state_needs_a_full_repaint_of_every_bo() {
        let d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            assert_eq!(
                pixels(&d.repaint_for(bo, true)),
                pixels(&full_region()),
                "bo {bo} claimed to hold content it never had"
            );
        }
    }

    #[test]
    fn a_painted_bo_needs_nothing_until_new_damage() {
        let mut d = ScanoutDamage::new(3, extent());
        frame(&mut d, 0, true, None);
        assert!(d.repaint_for(0, true).is_empty());
        // The others were never painted, so they still owe everything.
        assert_eq!(pixels(&d.repaint_for(1, true)), pixels(&full_region()));
    }

    #[test]
    fn unloadable_bo_always_needs_everything() {
        let mut d = ScanoutDamage::new(3, extent());
        frame(&mut d, 0, true, None);
        assert!(d.repaint_for(0, true).is_empty());
        assert_eq!(pixels(&d.repaint_for(0, false)), pixels(&full_region()));
    }

    #[test]
    fn damage_becomes_stale_in_every_other_bo() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        for bo in 0..3 {
            assert!(d.repaint_for(bo, true).is_empty());
        }
        let dmg = reg(&[rect(2, 2, 4, 4)]);
        d.add_damage(&dmg);
        // Paint it into BO 0 only.
        frame(&mut d, 0, true, None);
        assert!(d.repaint_for(0, true).is_empty(), "bo 0 painted it");
        for bo in [1, 2] {
            assert_eq!(
                pixels(&d.repaint_for(bo, true)),
                pixels(&dmg),
                "bo {bo} should still owe the damage it did not paint"
            );
        }
    }

    #[test]
    fn a_bo_skipped_for_several_frames_owes_the_union() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        let a = reg(&[rect(0, 0, 3, 3)]);
        let b = reg(&[rect(8, 8, 3, 3)]);
        let c = reg(&[rect(1, 12, 2, 2)]);
        // Three frames that all land in BO 0 and 1, never in 2.
        for dmg in [&a, &b, &c] {
            d.add_damage(dmg);
            frame(&mut d, 0, true, None);
        }
        let mut expect = a.clone();
        expect.union_with(&b);
        expect.union_with(&c);
        assert_eq!(pixels(&d.repaint_for(2, true)), pixels(&expect));
    }

    // ── transactions ─────────────────────────────────────────────────

    #[test]
    fn damage_arriving_between_submit_and_retire_survives() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        let early = reg(&[rect(0, 0, 4, 4)]);
        d.add_damage(&early);
        let repaint = d.repaint_for(0, true);
        d.commit_submitted(0, &repaint, &repaint);

        // A map/unmap/cursor move while the flip is in flight.
        let late = reg(&[rect(10, 10, 3, 3)]);
        d.add_damage(&late);
        d.retire_success();

        // BO 0 painted `early` but not `late`.
        assert_eq!(pixels(&d.repaint_for(0, true)), pixels(&late));
        // The others owe both.
        let mut both = early.clone();
        both.union_with(&late);
        assert_eq!(pixels(&d.repaint_for(1, true)), pixels(&both));
    }

    #[test]
    fn a_failed_submit_never_staged_so_the_next_tick_recomputes_the_same_repaint() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        d.add_damage(&reg(&[rect(1, 1, 5, 5)]));
        let first = d.repaint_for(0, true);
        // Submit fails: `commit_submitted` is never called.
        let second = d.repaint_for(0, true);
        assert_eq!(pixels(&first), pixels(&second));
        assert!(!d.has_staged_frame());
    }

    #[test]
    fn a_late_failure_restores_the_staged_damage() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        let dmg = reg(&[rect(1, 1, 5, 5)]);
        d.add_damage(&dmg);
        let repaint = d.repaint_for(0, true);
        d.commit_submitted(0, &repaint, &repaint);
        d.retire_failure();

        assert!(!d.has_staged_frame());
        // Nothing was presented, so every BO still owes it.
        for bo in 0..3 {
            assert_eq!(
                pixels(&d.repaint_for(bo, true)),
                pixels(&dmg),
                "bo {bo} lost damage that never reached the screen"
            );
        }
    }

    #[test]
    fn a_staged_frame_does_not_hide_damage_from_other_bos() {
        // Guards the `in_flight` term in `repaint_for`: damage that left
        // `pending` at submit but has not yet landed in `missing`.
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        let dmg = reg(&[rect(4, 4, 4, 4)]);
        d.add_damage(&dmg);
        let repaint = d.repaint_for(0, true);
        d.commit_submitted(0, &repaint, &repaint);
        assert_eq!(
            pixels(&d.repaint_for(1, true)),
            pixels(&dmg),
            "bo 1 must still owe damage staged against bo 0"
        );
    }

    // ── painted vs repaint ───────────────────────────────────────────

    #[test]
    fn a_wider_painted_region_clears_more_than_was_asked() {
        // bbox rendering paints a superset of the damage region; the extra is
        // legitimately no longer missing from that BO.
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        d.add_damage(&reg(&[rect(0, 0, 2, 2), rect(6, 6, 2, 2)]));
        let repaint = d.repaint_for(0, true);
        let bbox = reg(&[repaint.bounding_rect().expect("non-empty")]);
        d.commit_submitted(0, &repaint, &bbox);
        d.retire_success();
        assert!(d.repaint_for(0, true).is_empty());
        // And BO 1 owes only the real damage, not the bbox: `submitted_pending`
        // is the damage, not what was painted.
        assert_eq!(
            pixels(&d.repaint_for(1, true)),
            pixels(&reg(&[rect(0, 0, 2, 2), rect(6, 6, 2, 2)]))
        );
    }

    #[test]
    #[should_panic(expected = "painted region does not cover")]
    fn painting_less_than_asked_is_a_bug_and_says_so() {
        let mut d = ScanoutDamage::new(3, extent());
        d.add_damage(&reg(&[rect(0, 0, 8, 8)]));
        let repaint = d.repaint_for(0, true);
        let too_small = reg(&[rect(0, 0, 4, 4)]);
        d.commit_submitted(0, &repaint, &too_small);
    }

    #[test]
    #[should_panic(expected = "two frames staged at once")]
    fn staging_twice_without_retiring_is_a_bug_and_says_so() {
        let mut d = ScanoutDamage::new(3, extent());
        let r = d.repaint_for(0, true);
        d.commit_submitted(0, &r, &r);
        let r2 = d.repaint_for(1, true);
        d.commit_submitted(1, &r2, &r2);
    }

    // ── lifecycle ────────────────────────────────────────────────────

    #[test]
    fn invalidate_marks_everything_stale_and_drops_the_staged_frame() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        let r = d.repaint_for(0, true);
        d.commit_submitted(0, &r, &r);
        d.invalidate();
        assert!(!d.has_staged_frame());
        for bo in 0..3 {
            assert_eq!(pixels(&d.repaint_for(bo, true)), pixels(&full_region()));
        }
    }

    #[test]
    fn resize_rebuilds_the_per_bo_vector() {
        let mut d = ScanoutDamage::new(3, extent());
        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        let bigger = vk::Extent2D {
            width: 32,
            height: 32,
        };
        d.resize(2, bigger);
        assert_eq!(d.bo_count(), 2);
        for bo in 0..2 {
            assert_eq!(d.repaint_for(bo, true).area(), 32 * 32);
        }
    }

    #[test]
    fn an_empty_pool_never_panics() {
        let mut d = ScanoutDamage::new(0, extent());
        assert_eq!(d.bo_count(), 0);
        assert_eq!(pixels(&d.repaint_for(0, true)), pixels(&full_region()));
        d.add_rect(rect(0, 0, 4, 4));
        d.retire_success();
        d.retire_failure();
        d.invalidate();
    }

    #[test]
    fn retire_without_a_staged_frame_is_a_no_op() {
        let mut d = ScanoutDamage::new(3, extent());
        frame(&mut d, 0, true, None);
        let before = d.repaint_for(1, true);
        d.retire_success();
        d.retire_failure();
        assert_eq!(pixels(&d.repaint_for(1, true)), pixels(&before));
    }

    #[test]
    fn areas_report_what_telemetry_needs() {
        // `missing_area` is what makes the 4.8 gate observable: a clean run
        // where every repaint came from `pending` alone has not exercised the
        // per-BO model at all.
        let mut d = ScanoutDamage::new(3, extent());
        assert_eq!(d.pending_area(), 0);
        assert_eq!(d.missing_area(0), u64::from(W) * u64::from(H));
        assert_eq!(d.missing_area(99), 0, "out of range reads as nothing stale");

        for bo in 0..3 {
            frame(&mut d, bo, true, None);
        }
        assert_eq!(d.missing_area(0), 0);

        d.add_rect(rect(0, 0, 4, 5));
        assert_eq!(d.pending_area(), 20);
        frame(&mut d, 0, true, None);
        assert_eq!(d.pending_area(), 0, "attributed to a BO at submit");
        assert_eq!(d.missing_area(0), 0, "bo 0 painted it");
        assert_eq!(d.missing_area(1), 20, "bo 1 did not");
    }

    // ── rotation, against an independent shadow model ────────────────

    #[test]
    fn rotation_matches_an_independent_owed_set_model() {
        // The oracle: for each BO, the set of pixels damaged since that BO last
        // painted them. Maintained with plain pixel sets, deliberately sharing
        // no code with `ScanoutDamage` beyond `Region` itself.
        //
        // This is the test that would have caught the corruption the earlier
        // buffer-age attempts shipped: it is BO *rotation* that makes a partial
        // repaint unsafe, and rotation is what an always-Full integration test
        // cannot exercise.
        const BOS: usize = 3;
        let mut d = ScanoutDamage::new(BOS, extent());
        let mut owed: Vec<BTreeSet<(i32, i32)>> =
            (0..BOS).map(|_| pixels(&full_region())).collect();

        let mut seed = 0x5eed_0902_u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as u32
        };

        for step in 0..300u32 {
            // Some frames add damage, some are pure re-presents.
            if next() % 3 != 0 {
                let x = i32::try_from(next() % W).unwrap();
                let y = i32::try_from(next() % H).unwrap();
                let w = (next() % 5) + 1;
                let h = (next() % 5) + 1;
                let dmg = reg(&[rect(x, y, w, h)]);
                d.add_damage(&dmg);
                for o in &mut owed {
                    o.extend(pixels(&dmg));
                }
            }

            // Rotate, but not strictly: a real pool hands back whichever BO is
            // free, which is not always the round-robin next one.
            let bo = usize::try_from(next()).unwrap() % BOS;

            let repaint = d.repaint_for(bo, true);
            assert_eq!(
                pixels(&repaint),
                owed[bo],
                "step {step}: repaint for bo {bo} disagrees with the owed set"
            );

            if repaint.is_empty() {
                // Nothing to do; a real tick would EmptyDamage-skip.
                continue;
            }

            // Half the frames paint exactly the repaint region, half paint its
            // bounding box, so the `painted != repaint` path is exercised too.
            let painted = if next() % 2 == 0 {
                repaint.clone()
            } else {
                reg(&[repaint.bounding_rect().expect("non-empty")])
            };

            d.commit_submitted(bo, &repaint, &painted);

            if next() % 8 == 0 {
                // A flip that never landed.
                d.retire_failure();
                continue;
            }

            d.retire_success();
            for p in pixels(&painted) {
                owed[bo].remove(&p);
            }
        }
    }
}
