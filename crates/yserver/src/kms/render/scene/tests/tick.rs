use super::*;

#[test]
fn damage_audit_attribution_filters_to_episode_event_range() {
    let site = Location::caller();
    let ledger = VecDeque::from([
        DamageAuditLedgerEntry {
            id: 3,
            site,
            expected_area: vec![audit_rect(0, 0, 100, 100)],
            contributed_outputs: Vec::new(),
        },
        DamageAuditLedgerEntry {
            id: 4,
            site,
            expected_area: vec![audit_rect(0, 0, 100, 100)],
            contributed_outputs: vec![0],
        },
        DamageAuditLedgerEntry {
            id: 5,
            site,
            expected_area: vec![audit_rect(0, 0, 100, 100)],
            contributed_outputs: Vec::new(),
        },
        DamageAuditLedgerEntry {
            id: 6,
            site,
            expected_area: vec![audit_rect(0, 0, 100, 100)],
            contributed_outputs: Vec::new(),
        },
    ]);

    let found = ledger_candidates_for_tile(&ledger, 0, audit_rect(10, 10, 1, 1), 4, 6);

    assert!(!found.contains("3@"), "stale pre-episode event included");
    assert!(found.contains("4@"));
    assert!(found.contains(":contrib"));
    assert!(found.contains("5@"));
    assert!(found.contains(":missing"));
    assert!(!found.contains("6@"), "post-episode event included");
}

#[test]
fn damage_audit_partial_compose_is_detectable() {
    assert!(compose_submit_was_complete(
        ComposeSubmit {
            descriptor_count: 4
        },
        4
    ));
    assert!(!compose_submit_was_complete(
        ComposeSubmit {
            descriptor_count: 3
        },
        4
    ));
}

#[test]
fn copied_completion_requires_exact_job_and_paired_bo() {
    let waiting = InFlightStage::WaitingForRenderCompletion { job_id: 41 };
    assert!(copied_render_completion_matches(waiting, 2, 41, 2));
    assert!(!copied_render_completion_matches(waiting, 2, 42, 2));
    assert!(!copied_render_completion_matches(waiting, 2, 41, 1));
}

#[test]
fn copied_frame_cannot_retire_before_sink_kms_submission() {
    let waiting = InFlightStage::WaitingForRenderCompletion { job_id: 7 };
    assert!(!kms_retirement_matches(waiting, 1, 1));
    assert!(!copied_render_completion_matches(
        InFlightStage::KmsFlipPending,
        1,
        7,
        1,
    ));
}

#[test]
fn kms_retirement_requires_the_exact_paired_bo() {
    assert!(kms_retirement_matches(InFlightStage::KmsFlipPending, 1, 1,));
    assert!(!kms_retirement_matches(InFlightStage::KmsFlipPending, 1, 2,));
}

#[test]
fn global_drain_waits_and_releases_every_deferred_scene_resource() {
    let mut pending = VecDeque::from([(
        3,
        crate::kms::render::platform::FenceTicket::for_tests_stub(),
    )]);
    let mut failed = VecDeque::from([FailedSubmitBo {
        bo_idx: 7,
        pool_slot: 5,
        ticket: crate::kms::render::platform::FenceTicket::for_tests_stub(),
    }]);
    let mut waited = 0;
    let mut released = Vec::new();

    drain_deferred_scene_resources(
        &mut pending,
        &mut failed,
        |_| {
            waited += 1;
            true
        },
        |release| {
            released.push(release);
            true
        },
    );

    assert_eq!(waited, 2);
    assert!(pending.is_empty());
    assert!(failed.is_empty());
    assert_eq!(
        released,
        vec![
            DeferredSceneRelease::PoolSlot(3),
            DeferredSceneRelease::FailedSubmit {
                bo_idx: 7,
                pool_slot: 5,
            },
        ]
    );
}

#[test]
fn global_drain_retains_every_resource_whose_fence_wait_fails() {
    let mut pending = VecDeque::from([(
        3,
        crate::kms::render::platform::FenceTicket::for_tests_stub(),
    )]);
    let mut failed = VecDeque::from([FailedSubmitBo {
        bo_idx: 7,
        pool_slot: 5,
        ticket: crate::kms::render::platform::FenceTicket::for_tests_stub(),
    }]);
    let mut released = Vec::new();

    drain_deferred_scene_resources(
        &mut pending,
        &mut failed,
        |_| false,
        |release| {
            released.push(release);
            true
        },
    );

    assert!(released.is_empty());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, 3);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].bo_idx, 7);
    assert_eq!(failed[0].pool_slot, 5);
}

#[test]
fn tick_outcome_only_clears_dirty_for_compose_or_empty_skip() {
    assert!(TickOutcome::Composed.clears_scene_structure_dirty());
    assert!(TickOutcome::Skipped(TickSkipReason::EmptyDamage).clears_scene_structure_dirty());
    assert!(!TickOutcome::Skipped(TickSkipReason::PendingAcks).clears_scene_structure_dirty());
    assert!(!TickOutcome::Skipped(TickSkipReason::RetryDeadline).clears_scene_structure_dirty());
    assert!(!TickOutcome::Skipped(TickSkipReason::NoBO).clears_scene_structure_dirty());
    assert!(!TickOutcome::Skipped(TickSkipReason::NoPool).clears_scene_structure_dirty());
    assert!(!TickOutcome::Skipped(TickSkipReason::NothingPending).clears_scene_structure_dirty());
}

/// A `NothingPending` skip returns before `build_scene`, so it must not
/// count as walked — dormancy reconciliation would otherwise run on ids
/// this output never recorded and flag every armed window dormant.
#[test]
fn nothing_pending_skip_did_not_walk() {
    assert!(!TickOutcome::Skipped(TickSkipReason::NothingPending).walked());
    assert!(!TickOutcome::Skipped(TickSkipReason::PendingAcks).walked());
    assert!(TickOutcome::Skipped(TickSkipReason::EmptyDamage).walked());
    assert!(TickOutcome::Composed.walked());
}

/// Each input of the pre-walk predicate alone forces a walk; with none set
/// the tick may skip before walking. The dormant-only case is the one the
/// predicate exists for: `has_pending_presentation_damage` already
/// excludes dormant drawables, so it arrives here as `false`.
#[test]
fn walk_needed_for_each_input_alone_and_not_otherwise() {
    assert!(!walk_needed(false, false, false, false, false, false));
    assert!(
        walk_needed(true, false, false, false, false, false),
        "structure dirty"
    );
    assert!(
        walk_needed(false, true, false, false, false, false),
        "armed damage"
    );
    assert!(
        walk_needed(false, false, true, false, false, false),
        "first frame"
    );
    assert!(
        walk_needed(false, false, false, true, false, false),
        "owed repaint"
    );
    assert!(
        walk_needed(false, false, false, false, true, false),
        "structure rects"
    );
    assert!(
        walk_needed(false, false, false, false, false, true),
        "audit armed"
    );
}

/// The per-output form of the presentation input: a damaged drawable that
/// emitted pieces on output 0 only makes output 0 walk; one in no output's
/// set makes every output walk; nothing armed makes none walk.
#[test]
fn pending_presentation_is_decided_per_output_from_retained_pieces() {
    use crate::kms::render::store::DrawableId;
    use std::collections::HashSet;
    let a = DrawableId::for_tests(1);
    let b = DrawableId::for_tests(2);
    let out0: HashSet<DrawableId> = [a].into_iter().collect();
    let out1: HashSet<DrawableId> = HashSet::new();
    let all = [&out0, &out1];
    // `a` damaged, on output 0 only.
    assert!(pending_presentation_for_output(&[a], &out0, &all));
    assert!(!pending_presentation_for_output(&[a], &out1, &all));
    // `b` damaged but in no output's set ⇒ unknown ⇒ both walk.
    assert!(pending_presentation_for_output(&[b], &out0, &all));
    assert!(pending_presentation_for_output(&[b], &out1, &all));
    // Nothing armed ⇒ neither.
    assert!(!pending_presentation_for_output(&[], &out0, &all));
    assert!(!pending_presentation_for_output(&[], &out1, &all));
    // Spanning both outputs ⇒ both.
    let both0: HashSet<DrawableId> = [a].into_iter().collect();
    let both1: HashSet<DrawableId> = [a].into_iter().collect();
    let all2 = [&both0, &both1];
    assert!(pending_presentation_for_output(&[a], &both0, &all2));
    assert!(pending_presentation_for_output(&[a], &both1, &all2));
    // Structure dirty forces the walk regardless of the per-output answer.
    assert!(walk_needed(true, false, false, false, false, false));
}

/// The scheduler's view of a dormant drawable is exactly what the
/// predicate consumes: a `HiddenDamage`/`NoPieces` drawable with damage
/// reads as no armed id, so no output walks for it; the paint that re-arms
/// it flips both back.
#[test]
fn dormant_only_damage_does_not_walk_until_a_paint_rearms_it() {
    use crate::kms::render::store::{DormantReason, DrawableKind, DrawableStore, Storage};
    let mut store = DrawableStore::new();
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 8,
            height: 8,
        },
        vk::Format::B8G8R8A8_UNORM,
    );
    let id = store
        .allocate(0x700, DrawableKind::Window, 24, true, storage)
        .expect("allocate");
    store.damage(id, audit_rect(0, 0, 4, 4));
    assert!(store.has_pending_presentation_damage());
    // Reconciled as hidden-under-a-cover: pieces but no presented damage.
    let none = std::collections::HashSet::new();
    let pieces: std::collections::HashSet<_> = [id].into_iter().collect();
    store.reconcile_offscreen_no_draw(&none, &pieces);
    assert_eq!(
        store.get(id).map(|d| d.dormant),
        Some(Some(DormantReason::HiddenDamage))
    );
    assert!(!store.has_pending_presentation_damage());
    assert!(!walk_needed(
        false,
        store.has_pending_presentation_damage(),
        false,
        false,
        false,
        false
    ));
    // A new paint re-arms it and the predicate walks again.
    store.damage(id, audit_rect(2, 2, 2, 2));
    assert!(store.has_pending_presentation_damage());
    assert!(walk_needed(
        false,
        store.has_pending_presentation_damage(),
        false,
        false,
        false,
        false
    ));
}

/// Regression guard for the idle free-run fix: the empty-projection
/// force-compose must fire ONLY when a captured snapshot carries
/// non-empty damage. `peek_presentation_damage` returns `Some` even
/// for a clean (empty) region, so gating on `!snapshots.is_empty()`
/// force-composed every drawn window every vblank at idle. Gating on
/// real damage must (a) NOT force for a clean drawn window (→ idle
/// EmptyDamage skip) and (b) STILL force for a window that painted
/// but whose projection landed empty (the xfce submenu case).
#[test]
fn empty_projection_force_compose_gates_on_real_captured_damage() {
    use crate::kms::render::store::{DamageSnapshot, DrawableId};
    let id = DrawableId::for_tests(1);

    // No snapshots at all → no force.
    assert!(!snapshots_carry_damage(&[]));

    // Clean drawn window: peeked snapshot with an EMPTY region →
    // must NOT force (this was the idle free-run bug).
    let clean = DamageSnapshot {
        id,
        epoch: 1,
        region: RegionSet::new(),
    };
    assert!(!snapshots_carry_damage(std::slice::from_ref(&clean)));

    // Window that actually painted, projection landed empty:
    // non-empty captured damage → MUST still force (submenu case).
    let mut painted_region = RegionSet::new();
    painted_region.add(rect(0, 0, 4, 4));
    let painted = DamageSnapshot {
        id,
        epoch: 2,
        region: painted_region,
    };
    assert!(snapshots_carry_damage(std::slice::from_ref(&painted)));

    // Mixed (a clean + a painted) still forces.
    assert!(snapshots_carry_damage(&[clean, painted]));
}

#[test]
fn stub_scene_is_not_live_and_declines_tick() {
    let mut scene = SceneCompositor::stub();
    assert!(!scene.is_live());
    let core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut platform = PlatformBackend::for_tests();
    let mut telemetry = Telemetry::new();
    let windows = crate::kms::render::backend::WindowsMap::new();
    let err = scene
        .tick(
            &core,
            &mut store,
            &mut platform,
            &windows,
            &mut telemetry,
            None,
        )
        .expect_err("stub must reject tick");
    assert!(matches!(err, SceneError::NoVk));
}

#[test]
fn mark_scene_structure_dirty_is_idempotent() {
    let mut scene = SceneCompositor::stub();
    scene.scene_structure_dirty = false;
    scene.mark_scene_structure_dirty();
    assert!(scene.scene_structure_dirty);
    scene.mark_scene_structure_dirty();
    assert!(scene.scene_structure_dirty);
}

/// Stage 4c.1 — the plural setter sets `scene_structure_dirty`
/// even on the stub-mode compositor (mirrors the singular
/// setter's early-return shape).
#[test]
fn mark_scene_structure_damage_rects_sets_dirty_on_stub() {
    let mut scene = SceneCompositor::stub();
    scene.scene_structure_dirty = false;
    scene.mark_scene_structure_damage_rects(&[rect(0, 0, 10, 10)]);
    assert!(scene.scene_structure_dirty);
}

#[test]
fn root_overlay_toggle_marks_structure_damage() {
    let mut sc = SceneCompositor::stub();
    assert!(!sc.scene_structure_dirty);
    sc.root_overlay_toggle(
        yserver_protocol::x11::ClientId(1),
        0xffffff,
        &[ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 5, y: 5 },
            extent: ash::vk::Extent2D {
                width: 20,
                height: 20,
            },
        }],
    );
    assert!(
        sc.scene_structure_dirty,
        "overlay mutation must mark structure damage"
    );
    assert!(!sc.root_overlay.is_empty());
}
