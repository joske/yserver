//! Structural damage derived by diffing the emitted scene against the last
//! presented one.
//!
//! Step 2 of
//! `docs/superpowers/plans/2026-09-01-damage-derived-scene-repaint-plan.md`.
//!
//! # Why a diff and not wlroots' mutator hook
//!
//! wlroots derives damage at mutation time: `wlr_scene_node_set_position`
//! assigns, then calls `scene_node_update`, which reads the node's *cached
//! previous* visible region. That rests on a persistent scene graph.
//!
//! yserver has none. `build_scene` is a stateless walk, called fresh per output
//! per frame; it reads `top_level_order`, the windows map, shapes and the store,
//! and returns a `SceneBuild` that is consumed and dropped. There is no node to
//! cache a region on, and building a persistent mirror would mean a second
//! source of truth for window state, invalidated by a dozen inputs — five of
//! which are mutated today with no damage call at all. A hand-maintained cache
//! across that surface would be wrong, and its failure mode is a stale pixel.
//!
//! So: keep the previous frame's emitted scene and diff against it. Every input
//! the walk reads feeds `build_scene`, so every one shows up in the diff for
//! free, and there is no second source of truth because the "cache" is a
//! snapshot of the render input.
//!
//! It also reproduces the design's invariant by construction rather than by
//! argument. For a drag: the moved window's entry changed, so damage is its old
//! region ∪ its new one — and the content revealed beneath lies inside the old
//! region, so the participants below need no damage of their own.
//!
//! # What it deliberately does not see
//!
//! Four producers stay separate, and a reader who takes "damage falls out of
//! scene changes" literally will look for them here and not find them:
//!
//! - **Content damage** — the largest. A window painting into its own storage
//!   changes no scene metadata at all; that damage comes from the store's
//!   per-drawable presentation damage, projected in `build_scene`.
//! - **The cursor.** It is an ordinary draw in the scene list, so it *could* be
//!   an ordinary participant here — the plan left that open, and the answer is
//!   no. Its damage is already tick-owned and transactional
//!   (`last_present_cursor_rect`/`version`, retired through `PendingAck`), and
//!   that machinery is what correctly handles the HW-plane fast path and the
//!   cursorless-hide handoff frame. Emitting a cursor presence would create a
//!   second source of truth for the same pixels, and would make
//!   `prev_presented` differ on every frame the pointer moves — which muddies
//!   the one question this diff exists to answer.
//! - **The root `IncludeInferiors` XOR overlay**, which is not a draw in the
//!   scene list at all but a separate logic-op pass.
//! - **The empty-projection force-compose**, which exists so a paint whose
//!   projection landed empty can retire at all.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the signature accessors land ahead of their consumers"
    )
)]

use std::collections::HashMap;

use ash::vk;

use super::region::Region;

/// What kind of thing a scene participant is.
///
/// Part of the identity because the same xid can legitimately appear in more
/// than one role — the root is both a window in the windows map and the bottom
/// draw of the scene.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum SceneRole {
    Root,
    Window,
}

/// Identity of a scene participant, stable across frames.
///
/// `generation` is the store's `DrawableId`, and it is not decoration: an xid
/// can be destroyed and reused, and Vulkan handles are recycled too, so without
/// it a destroyed-and-recreated window at the same geometry compares **equal**
/// to the one it replaced and the diff reports no change for what is a
/// different window.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ParticipantId {
    pub(crate) role: SceneRole,
    pub(crate) xid: u32,
    pub(crate) generation: u64,
}

/// Everything about a participant that changes what its pixels look like
/// without changing where they are.
///
/// Compared bit-for-bit, so `f32` fields are held as bits — `f32` is not `Eq`,
/// and these values are derived deterministically from integers anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PresenceSignature {
    /// The **sample-side** view the scene binds, not the raw storage view. A
    /// redirect swap or a storage reallocation changes this while geometry stays
    /// put, and both must damage.
    pub(crate) sample_view: vk::ImageView,
    src_origin: [u32; 2],
    src_size: [u32; 2],
    pub(crate) alpha_passthrough: bool,
}

impl PresenceSignature {
    pub(crate) fn new(
        sample_view: vk::ImageView,
        src_origin: [f32; 2],
        src_size: [f32; 2],
        alpha_passthrough: bool,
    ) -> Self {
        Self {
            sample_view,
            src_origin: [src_origin[0].to_bits(), src_origin[1].to_bits()],
            src_size: [src_size[0].to_bits(), src_size[1].to_bits()],
            alpha_passthrough,
        }
    }
}

/// One participant's footprint in one frame's scene.
///
/// The `region` is the union of that participant's **placement** rects — where
/// the window sits (rect ∩ ancestors ∩ shape), not where it is visible — so a
/// shaped window emitting one draw per shape rect is **one** presence, which is
/// what makes "did this participant change" a meaningful question rather than a
/// per-quad one.
///
/// Step 1 clips emitted draws to what nothing above covers. The diff must keep
/// reading placement: if `region` followed visibility, every window uncovered
/// by a move above it would read as "moved" and damage its whole footprint on
/// every restack of anything. wlroots damages the mover's old ∪ new only; the
/// uncovered nodes are repainted because they lie inside it. So `visible` is
/// carried separately and [`structural_damage`] never reads it — it is there
/// for step 1's later stage, which clips content damage to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScenePresence {
    pub(crate) id: ParticipantId,
    /// Placement (unclipped by occlusion), output-local — the damage-side
    /// summary, under the 32-box cap (a superset is safe when it is *added*).
    pub(crate) region: Region,
    /// The exact placement rects, in EMISSION order — the participant's
    /// **identity** for the diff. `region` cannot serve: past the cap it
    /// collapses towards its bounding box, so two shapes of more than 32
    /// fragments with the same extents compared EQUAL and a shape change went
    /// undamaged (codex, post-merge review of `02bafec3`). Since step 2 a shape
    /// change only wakes the tick, so this comparison is the only thing that
    /// repaints it.
    ///
    /// Not canonicalised: the order is deterministic for an unchanged tree
    /// (shape rects come from the stored `shape_bounding` list, an unshaped
    /// node has one rect), and a client that re-specifies the same shape in a
    /// different order reads as `moved` — over-damage, the safe direction. A
    /// per-node sort on the walk's hot path was measurable (+13% on the bench)
    /// and bought nothing.
    pub(crate) place: Vec<vk::Rect2D>,
    /// What of `region` nothing above covers. Empty for a fully hidden
    /// participant, which is **still a participant**. Not read by the diff.
    pub(crate) visible: Region,
    pub(crate) signature: PresenceSignature,
}

/// Build a presence from a participant's placement rects.
///
/// `place` is the node decision's exact rect list, taken by value — the
/// decision is done with it once the node has emitted and claimed, so the
/// presence owns it without a copy. The union here is damage-side, so the
/// region cap's over-approximation is safe. `None` when the participant
/// occupies nothing, which is not the same as being hidden: a hidden
/// participant has a non-empty `place` and an empty `visible`.
pub(crate) fn presence_from_place(
    place: Vec<vk::Rect2D>,
    visible: Region,
    id: ParticipantId,
    signature: PresenceSignature,
) -> Option<ScenePresence> {
    let region = Region::from_rects(place.iter().copied());
    if region.is_empty() {
        return None;
    }
    Some(ScenePresence {
        id,
        region,
        place,
        visible,
        signature,
    })
}

/// Damage owed because the scene's *structure* changed since the last presented
/// frame.
///
/// For every participant that appeared, vanished, moved or changed what it
/// samples: its old region ∪ its new one. For a change of **stacking** alone:
/// only the pixels that can actually change, which are where two participants
/// whose front-to-back order flipped **overlap** — `P.region ∩ Q.region` for
/// every such pair.
///
/// Order is compared by rank among the participants **common** to both frames.
/// The first version of this damaged the whole region of every participant
/// whose rank index moved, which is everything the raised window jumped over:
/// mpv (0.14 of the screen) raised over a half-screen terminal damaged ~0.64 of
/// the output, crossed the 0.6 clip threshold and forced Full frames on every
/// restack (z400, e16 workload, 2026-09-03). A window jumped over changes only
/// where the mover covers or uncovers it.
pub(crate) fn structural_damage(prev: &[ScenePresence], now: &[ScenePresence]) -> Region {
    let mut damage = Region::new();

    let prev_by_id: HashMap<ParticipantId, &ScenePresence> =
        prev.iter().map(|p| (p.id, p)).collect();
    let now_by_id: HashMap<ParticipantId, &ScenePresence> = now.iter().map(|p| (p.id, p)).collect();

    // Rank among common participants, which is what stacking order means here:
    // a participant that appeared or vanished is already damaged in full, so its
    // effect on everyone else's absolute index is not itself a change.
    let common_rank = |list: &[ScenePresence], other: &HashMap<ParticipantId, &ScenePresence>| {
        let mut ranks = HashMap::new();
        for p in list.iter().filter(|p| other.contains_key(&p.id)) {
            let next = ranks.len();
            ranks.insert(p.id, next);
        }
        ranks
    };
    let prev_rank = common_rank(prev, &now_by_id);
    let now_rank = common_rank(now, &prev_by_id);

    // Participants whose rank index moved. A pair whose relative order flipped
    // must contain AT LEAST ONE of these (if neither index moved, their order
    // did not change) — but not necessarily two: prev [A,B,C] → now [C,B,A]
    // leaves B at rank 1 while it flips against both A and C. So every
    // rank-changed participant is compared against every other common
    // participant, O(k·n), not only against the other rank-changed ones
    // (codex, post-merge review of `02bafec3`, finding 2).
    let mut rank_changed: Vec<&ScenePresence> = Vec::new();
    for p in now {
        match prev_by_id.get(&p.id) {
            None => damage.union_with(&p.region),
            Some(old) => {
                // Identity is the exact place list; `region` alone collapses
                // past the cap (see `ScenePresence::place`).
                let moved = old.place != p.place || old.region != p.region;
                let resampled = old.signature != p.signature;
                if moved || resampled {
                    damage.union_with(&old.region);
                    damage.union_with(&p.region);
                }
                if prev_rank.get(&p.id) != now_rank.get(&p.id) {
                    rank_changed.push(p);
                }
            }
        }
    }
    let changed_ids: std::collections::HashSet<ParticipantId> =
        rank_changed.iter().map(|p| p.id).collect();
    for p in &rank_changed {
        for q in now
            .iter()
            .filter(|q| q.id != p.id && prev_by_id.contains_key(&q.id))
        {
            // A pair with both members rank-changed is visited from both sides;
            // handle it once, from the member that now ranks lower.
            if changed_ids.contains(&q.id) && now_rank[&p.id] > now_rank[&q.id] {
                continue;
            }
            let flipped =
                (prev_rank[&p.id] < prev_rank[&q.id]) != (now_rank[&p.id] < now_rank[&q.id]);
            if !flipped {
                continue;
            }
            // New regions: for a pure restack old == new, and a participant that
            // also moved or resampled is already owed in full above. The
            // intersection of two capped regions can itself exceed the box cap
            // and collapse to its bounding box — a superset, which on the damage
            // side is the safe direction.
            let mut overlap = p.region.clone();
            overlap.intersect_with(&q.region);
            damage.union_with(&overlap);
        }
    }
    for p in prev {
        if !now_by_id.contains_key(&p.id) {
            damage.union_with(&p.region);
        }
    }
    damage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    fn id(xid: u32) -> ParticipantId {
        ParticipantId {
            role: SceneRole::Window,
            xid,
            generation: u64::from(xid),
        }
    }

    fn sig() -> PresenceSignature {
        PresenceSignature::new(vk::ImageView::null(), [0.0, 0.0], [1.0, 1.0], false)
    }

    fn presence(xid: u32, r: vk::Rect2D) -> ScenePresence {
        ScenePresence {
            id: id(xid),
            region: Region::from_rect(r),
            place: vec![r],
            visible: Region::from_rect(r),
            signature: sig(),
        }
    }

    fn pixels(r: &Region) -> std::collections::BTreeSet<(i32, i32)> {
        let mut s = std::collections::BTreeSet::new();
        for rc in r.rects() {
            for y in rc.offset.y..rc.offset.y + rc.extent.height as i32 {
                for x in rc.offset.x..rc.offset.x + rc.extent.width as i32 {
                    s.insert((x, y));
                }
            }
        }
        s
    }

    // ── the diff ─────────────────────────────────────────────────────

    #[test]
    fn an_unchanged_scene_owes_nothing() {
        let scene = vec![
            presence(1, rect(0, 0, 100, 100)),
            presence(2, rect(5, 5, 10, 10)),
        ];
        assert!(structural_damage(&scene, &scene).is_empty());
    }

    #[test]
    fn a_moved_window_damages_old_and_new() {
        // The drag case, and the whole point of the step: today this damages the
        // entire output.
        let before = vec![presence(1, rect(0, 0, 20, 20))];
        let after = vec![presence(1, rect(50, 50, 20, 20))];
        let d = structural_damage(&before, &after);
        let mut expect = Region::from_rect(rect(0, 0, 20, 20));
        expect.union_with(&Region::from_rect(rect(50, 50, 20, 20)));
        assert_eq!(pixels(&d), pixels(&expect));
    }

    #[test]
    fn a_mapped_window_damages_only_itself() {
        let before = vec![presence(1, rect(0, 0, 200, 200))];
        let after = vec![
            presence(1, rect(0, 0, 200, 200)),
            presence(2, rect(10, 10, 30, 30)),
        ];
        assert_eq!(
            pixels(&structural_damage(&before, &after)),
            pixels(&Region::from_rect(rect(10, 10, 30, 30)))
        );
    }

    #[test]
    fn an_unmapped_window_damages_where_it_was() {
        // And that is sufficient for what is revealed beneath it: the revealed
        // content lies inside the region it vacated.
        let before = vec![
            presence(1, rect(0, 0, 200, 200)),
            presence(2, rect(10, 10, 30, 30)),
        ];
        let after = vec![presence(1, rect(0, 0, 200, 200))];
        assert_eq!(
            pixels(&structural_damage(&before, &after)),
            pixels(&Region::from_rect(rect(10, 10, 30, 30)))
        );
    }

    #[test]
    fn resampling_the_same_geometry_still_damages() {
        // A redirect swap or a storage realloc keeps the window exactly where it
        // is and changes what it samples. Identity follows the host, so this is
        // a signature change rather than a new participant.
        let before = vec![presence(1, rect(0, 0, 20, 20))];
        let mut after = before.clone();
        after[0].signature =
            PresenceSignature::new(vk::ImageView::null(), [0.25, 0.0], [0.5, 1.0], false);
        assert_eq!(
            pixels(&structural_damage(&before, &after)),
            pixels(&Region::from_rect(rect(0, 0, 20, 20)))
        );
    }

    #[test]
    fn an_alpha_flip_damages() {
        let before = vec![presence(1, rect(0, 0, 20, 20))];
        let mut after = before.clone();
        after[0].signature =
            PresenceSignature::new(vk::ImageView::null(), [0.0, 0.0], [1.0, 1.0], true);
        assert!(!structural_damage(&before, &after).is_empty());
    }

    #[test]
    fn xid_reuse_is_not_mistaken_for_the_same_window() {
        // Same xid, same geometry, different drawable: a destroyed and recreated
        // window. Without the generation in the identity this reports no change.
        let before = vec![presence(1, rect(0, 0, 20, 20))];
        let mut after = before.clone();
        after[0].id.generation = 999;
        assert_eq!(
            pixels(&structural_damage(&before, &after)),
            pixels(&Region::from_rect(rect(0, 0, 20, 20))),
            "a recreated window must damage its region"
        );
    }

    #[test]
    fn the_same_xid_in_two_roles_is_two_participants() {
        let root = ScenePresence {
            id: ParticipantId {
                role: SceneRole::Root,
                xid: 1,
                generation: 1,
            },
            region: Region::from_rect(rect(0, 0, 100, 100)),
            place: vec![rect(0, 0, 100, 100)],
            visible: Region::from_rect(rect(0, 0, 100, 100)),
            signature: sig(),
        };
        let win = presence(1, rect(0, 0, 100, 100));
        assert_ne!(root.id, win.id);
        assert!(structural_damage(&[root.clone(), win.clone()], &[root, win]).is_empty());
    }

    // ── stacking ─────────────────────────────────────────────────────

    #[test]
    fn a_raise_damages_only_where_it_overlaps_what_it_jumped_over() {
        // Overlapping neighbours: a raised over b and c changes pixels only
        // where a and b, and a and c, overlap — b ∩ c did not reorder.
        let a = presence(1, rect(0, 0, 30, 10));
        let b = presence(2, rect(20, 0, 30, 10));
        let c = presence(3, rect(40, 0, 30, 10));
        // Nothing reordered: nothing owed.
        assert!(
            structural_damage(
                &[a.clone(), b.clone(), c.clone()],
                &[a.clone(), b.clone(), c.clone()]
            )
            .is_empty()
        );
        let d = structural_damage(
            &[a.clone(), b.clone(), c.clone()],
            &[b.clone(), c.clone(), a],
        );
        // a ∩ b = [20,30), a ∩ c = ∅ (a ends at 30, c starts at 40).
        assert_eq!(pixels(&d), pixels(&Region::from_rect(rect(20, 0, 10, 10))));
    }

    #[test]
    fn a_raised_window_damages_only_what_it_overlaps() {
        // The z400 e16 restack regression (finding 2026-09-03): mpv raised over
        // a half-screen terminal must damage mpv ∩ terminal, not mpv ∪ terminal —
        // the union crossed the 0.6 clip threshold and forced Full frames.
        let a = presence(1, rect(0, 0, 100, 100));
        let b = presence(2, rect(50, 50, 100, 100));
        let c = presence(3, rect(300, 300, 20, 20));
        let d = structural_damage(
            &[a.clone(), b.clone(), c.clone()],
            &[b.clone(), a.clone(), c.clone()],
        );
        assert_eq!(
            pixels(&d),
            pixels(&Region::from_rect(rect(50, 50, 50, 50))),
            "exactly a ∩ b"
        );
        let mut union = Region::from_rect(rect(0, 0, 100, 100));
        union.union_with(&Region::from_rect(rect(50, 50, 100, 100)));
        assert_ne!(pixels(&d), pixels(&union), "not the coarse a ∪ b");
        assert!(
            !d.intersects_rect(rect(300, 300, 20, 20)),
            "c never reordered relative to anything it overlaps"
        );
    }

    #[test]
    fn a_restack_past_a_non_overlapping_window_damages_nothing() {
        let a = presence(1, rect(0, 0, 10, 10));
        let c = presence(3, rect(40, 0, 10, 10));
        let d = structural_damage(&[a.clone(), c.clone()], &[c, a]);
        assert!(
            d.is_empty(),
            "disjoint windows swapping order change no pixel"
        );
    }

    #[test]
    fn a_window_moved_past_several_damages_each_overlap() {
        // p jumps from the bottom to the top over q1, q2, q3. q1 and q2 overlap
        // each other but keep their relative order, so q1 ∩ q2 must NOT be
        // damaged where it lies outside p.
        let p = presence(1, rect(0, 0, 50, 50));
        let q1 = presence(2, rect(40, 0, 50, 20)); // p ∩ q1 = (40,0,10,20)
        let q2 = presence(3, rect(40, 10, 50, 20)); // p ∩ q2 = (40,10,10,20); q1 ∩ q2 ≠ ∅
        let q3 = presence(4, rect(0, 40, 20, 50)); // p ∩ q3 = (0,40,20,10)
        let d = structural_damage(
            &[p.clone(), q1.clone(), q2.clone(), q3.clone()],
            &[q1.clone(), q2.clone(), q3.clone(), p.clone()],
        );
        let mut expect = Region::from_rect(rect(40, 0, 10, 20));
        expect.union_with(&Region::from_rect(rect(40, 10, 10, 20)));
        expect.union_with(&Region::from_rect(rect(0, 40, 20, 10)));
        assert_eq!(pixels(&d), pixels(&expect));
        // q1 ∩ q2 outside p: x in [50,90), y in [10,20) — untouched.
        assert!(!d.intersects_rect(rect(50, 10, 40, 10)));
    }

    #[test]
    fn restack_plus_move_is_still_old_union_new() {
        // The mover moved AND reordered: the move rule already owes old ∪ new,
        // and the overlap rule must not take anything away from that.
        let a_old = presence(1, rect(0, 0, 10, 10));
        let a_new = presence(1, rect(100, 100, 10, 10));
        let b = presence(2, rect(5, 5, 10, 10));
        let d = structural_damage(&[a_old.clone(), b.clone()], &[b.clone(), a_new.clone()]);
        let mut expect = Region::from_rect(rect(0, 0, 10, 10));
        expect.union_with(&Region::from_rect(rect(100, 100, 10, 10)));
        assert_eq!(pixels(&d), pixels(&expect));
    }

    /// Codex, post-merge review of `02bafec3` (finding 2): a batched restack
    /// can leave a participant at the SAME rank while its order relative to
    /// others flips. prev [A,B,C] → now [C,B,A]: B keeps rank 1 yet is now below
    /// C and above A. Comparing only pairs where BOTH ranks moved (A,C) misses
    /// both overlaps involving B, and when A and C are disjoint the damage is
    /// empty despite a visible stacking change.
    #[test]
    fn a_batched_restack_damages_the_pivot_that_kept_its_rank() {
        let a = presence(1, rect(0, 0, 30, 10));
        let b = presence(2, rect(20, 0, 30, 10)); // a ∩ b = [20,30), b ∩ c = [40,50)
        let c = presence(3, rect(40, 0, 30, 10)); // a ∩ c = ∅
        let d = structural_damage(
            &[a.clone(), b.clone(), c.clone()],
            &[c.clone(), b.clone(), a.clone()],
        );
        let mut expect = Region::from_rect(rect(20, 0, 10, 10));
        expect.union_with(&Region::from_rect(rect(40, 0, 10, 10)));
        assert_eq!(
            pixels(&d),
            pixels(&expect),
            "b flipped against both a and c: both overlaps are owed"
        );
    }

    fn shape(xid: u32, rects: &[vk::Rect2D]) -> ScenePresence {
        presence_from_place(rects.to_vec(), Region::new(), id(xid), sig()).expect("non-empty shape")
    }

    /// Codex finding 3: `region` is a capped `Region`, so two shapes with more
    /// than 32 fragments and the same bounding box compare EQUAL after the
    /// collapse. Shape changes only wake the tick since step 2, so a missed
    /// comparison is a missed repaint. Identity must be the exact rect list.
    #[test]
    fn a_shape_change_beyond_the_region_cap_is_still_a_move() {
        // 40 disjoint 2×2 rects. `Region::from_rects` adds one at a time and
        // collapses the accumulated prefix to its bounding box the moment it
        // exceeds 32 boxes, so rects 0..=32 end up as ONE box in both shapes
        // when their first and last rects agree; rects 1..32 differ by a pixel
        // and are exactly what the collapse erases. Rects 33..40 are identical.
        let mut one = vec![rect(0, 0, 2, 2)];
        let mut two = vec![rect(0, 0, 2, 2)];
        for i in 1..32 {
            one.push(rect(i * 4, 0, 2, 2));
            two.push(rect(i * 4 + 1, 0, 2, 2));
        }
        for i in 32..40 {
            one.push(rect(i * 4, 0, 2, 2));
            two.push(rect(i * 4, 0, 2, 2));
        }
        let before = shape(7, &one);
        let after = shape(7, &two);
        assert_eq!(
            before.region, after.region,
            "precondition: the capped regions are indistinguishable"
        );
        let d = structural_damage(std::slice::from_ref(&before), std::slice::from_ref(&after));
        assert!(!d.is_empty(), "a different shape must owe damage");
        assert!(
            d.intersects_rect(rect(4, 0, 2, 2)),
            "covers a fragment that is in one shape and not the other"
        );
        // Identical shapes owe nothing.
        let same = structural_damage(
            std::slice::from_ref(&before),
            std::slice::from_ref(&shape(7, &one)),
        );
        assert!(same.is_empty());
    }

    #[test]
    fn a_map_does_not_read_as_a_restack_of_everything() {
        // Rank is computed among COMMON participants, so inserting a new
        // participant in the middle must not flag every one above it. Otherwise
        // opening a window damages the whole stack and step 2 buys nothing.
        let a = presence(1, rect(0, 0, 10, 10));
        let b = presence(2, rect(20, 0, 10, 10));
        let new = presence(9, rect(60, 0, 5, 5));
        let d = structural_damage(&[a.clone(), b.clone()], &[a, new, b]);
        assert_eq!(
            pixels(&d),
            pixels(&Region::from_rect(rect(60, 0, 5, 5))),
            "only the new participant should be damaged"
        );
    }

    // ── presence construction ────────────────────────────────────────

    #[test]
    fn a_shaped_window_is_one_participant_with_a_unioned_region() {
        // One place rect per shape rect, one presence. Otherwise "did this
        // window change" becomes a per-quad question and a shape edit reads as
        // several participants appearing and vanishing.
        let place = [rect(0, 0, 10, 10), rect(20, 0, 10, 10)];
        let p = presence_from_place(place.to_vec(), Region::new(), id(1), sig()).expect("placed");
        let mut expect = Region::from_rect(rect(0, 0, 10, 10));
        expect.union_with(&Region::from_rect(rect(20, 0, 10, 10)));
        assert_eq!(pixels(&p.region), pixels(&expect));
    }

    #[test]
    fn a_participant_with_no_place_has_no_presence() {
        assert!(presence_from_place(Vec::new(), Region::new(), id(1), sig()).is_none());
    }

    /// Step 1: the diff reads placement, never visibility. Two frames that
    /// differ only in what is covered owe nothing — the cover's own move is
    /// what damages the uncovered pixels.
    #[test]
    fn a_visibility_change_alone_owes_nothing() {
        let before = vec![ScenePresence {
            id: id(1),
            region: Region::from_rect(rect(0, 0, 100, 100)),
            place: vec![rect(0, 0, 100, 100)],
            visible: Region::from_rect(rect(0, 0, 100, 100)),
            signature: sig(),
        }];
        let after = vec![ScenePresence {
            id: id(1),
            region: Region::from_rect(rect(0, 0, 100, 100)),
            place: vec![rect(0, 0, 100, 100)],
            visible: Region::new(),
            signature: sig(),
        }];
        assert!(structural_damage(&before, &after).is_empty());
        assert!(structural_damage(&after, &before).is_empty());
    }

    #[test]
    fn first_frame_against_nothing_damages_everything_present() {
        let now = vec![
            presence(1, rect(0, 0, 100, 100)),
            presence(2, rect(10, 10, 20, 20)),
        ];
        let d = structural_damage(&[], &now);
        assert_eq!(pixels(&d), pixels(&Region::from_rect(rect(0, 0, 100, 100))));
    }
}
